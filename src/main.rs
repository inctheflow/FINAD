use axum::{
    extract::{FromRequestParts, Multipart, State},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bcrypt::{hash, verify, DEFAULT_COST};
use csv;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;

type AppState = Arc<Mutex<Connection>>;

// ── Existing data structs ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    date: String,
    amount: f64,
    description: String,
    #[serde(default)]
    category: String,
}

// ── Auth structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: i64,
    exp: usize,
}

#[derive(Deserialize)]
struct AuthRequest {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
}

#[derive(Deserialize)]
struct AccountUpdateRequest {
    phone: Option<String>,
    security_question: Option<String>,
    security_answer: Option<String>,
}

#[derive(Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

struct AuthUser {
    user_id: i64,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Missing Authorization header"})),
            ))?;

        let token = auth_header.strip_prefix("Bearer ").ok_or((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Invalid Authorization format, expected: Bearer <token>"})),
        ))?;

        let secret = env::var("JWT_SECRET").unwrap_or_else(|_| "change_me_in_production".to_string());

        let token_data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &Validation::default(),
        )
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid or expired token"})),
            )
        })?;

        Ok(AuthUser { user_id: token_data.claims.sub })
    }
}

// ── JWT helper ────────────────────────────────────────────────────────────────

fn create_token(user_id: i64) -> Result<String, jsonwebtoken::errors::Error> {
    let secret = env::var("JWT_SECRET").unwrap_or_else(|_| "change_me_in_production".to_string());
    let exp = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::days(7))
        .expect("valid timestamp")
        .timestamp() as usize;
    let claims = Claims { sub: user_id, exp };
    encode(&Header::default(), &claims, &EncodingKey::from_secret(secret.as_bytes()))
}

// ── Analytics structs ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TopExpense {
    date: String,
    description: String,
    amount: f64,
    category: String,
}

// ── Teller structs ────────────────────────────────────────────────────────────

/// Sent by the frontend after the user completes the Teller Connect OAuth flow.
#[derive(Deserialize)]
struct TellerEnrollRequest {
    access_token: String,   // from Teller Connect onSuccess callback
    enrollment_id: String,  // enrollment.id from the same callback
    institution_name: String,
}

#[derive(Serialize)]
struct TellerAccountRow {
    id: i64,
    institution_name: String,
    account_name: String,
    account_type: String,
    last_four: Option<String>,
}

// ── Cash entry structs ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CashEntryRequest {
    date: String,
    amount: f64,
    entry_type: String, // "income" or "expense"
    description: String,
}

#[derive(Serialize)]
struct CashEntry {
    id: i64,
    date: String,
    amount: f64,
    entry_type: String,
    description: String,
}

// ── Regex & categorization ────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

// Apple Card: DATE  DESCRIPTION  XX%  $CASHBACK  $AMOUNT
static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(\d{2}/\d{2}/\d{4})\s+([A-Z0-9 \*\.\-]+?)\s+\d+%\s+\$[\d\.]+\s+\$(-?[\d\.]+)",
    )
    .unwrap()
});

// Discover: MM/DD/YY  MM/DD/YY  DESCRIPTION (may wrap lines)  $  AMOUNT
static RE_DISCOVER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?s)(\d{2}/\d{2}/\d{2})\s+\d{2}/\d{2}/\d{2}\s+(.+?)\$\s+(-?[\d,]+\.\d{2})",
    )
    .unwrap()
});

fn discover_date(date: &str) -> String {
    let parts: Vec<&str> = date.split('/').collect();
    if parts.len() == 3 && parts[2].len() == 2 {
        format!("{}/{}/20{}", parts[0], parts[1], parts[2])
    } else {
        date.to_string()
    }
}

fn categorize(description: &str) -> String {
    let rules: &[(&str, &str)] = &[
        // ── Dining ───────────────────────────────────────────────────────────
        ("akira ramen",         "dining"),
        ("swadesh",             "dining"),
        ("kong",                "dining"),
        ("tst*",                "dining"),
        ("potomac pizza",       "dining"),
        ("lao sze chuan",       "dining"),
        ("starbucks",           "dining"),
        ("mcdonalds",           "dining"),
        ("mcdonald",            "dining"),
        ("bakery momo",         "dining"),
        ("tous les jours",      "dining"),
        ("spice",               "dining"),
        ("kung fu tea",         "dining"),
        ("chipotle",            "dining"),
        ("chick-fil-a",         "dining"),
        ("chickfila",           "dining"),
        ("subway",              "dining"),
        ("panera",              "dining"),
        ("domino",              "dining"),
        ("pizza hut",           "dining"),
        ("papa john",           "dining"),
        ("taco bell",           "dining"),
        ("burger king",         "dining"),
        ("wendy",               "dining"),
        ("dunkin",              "dining"),
        ("doordash",            "dining"),
        ("grubhub",             "dining"),
        ("ubereats",            "dining"),
        ("uber eats",           "dining"),
        ("noodle",              "dining"),
        ("sushi",               "dining"),
        ("ramen",               "dining"),
        ("pho",                 "dining"),
        ("bbq",                 "dining"),
        ("grill",               "dining"),
        ("diner",               "dining"),
        ("cafe",                "dining"),
        ("coffee",              "dining"),
        ("bakery",              "dining"),
        ("boba",                "dining"),
        ("smoothie",            "dining"),
        ("restaurant",          "dining"),
        ("eatery",              "dining"),
        ("kitchen",             "dining"),
        ("bar & grill",         "dining"),
        // ── Groceries ────────────────────────────────────────────────────────
        ("h mart",              "groceries"),
        ("hmart",               "groceries"),
        ("safeway",             "groceries"),
        ("walmart",             "groceries"),
        ("target",              "groceries"),
        ("costco",              "groceries"),
        ("bjs",                 "groceries"),
        ("whole foods",         "groceries"),
        ("trader joe",          "groceries"),
        ("aldi",                "groceries"),
        ("kroger",              "groceries"),
        ("giant",               "groceries"),
        ("food lion",           "groceries"),
        ("harris teeter",       "groceries"),
        ("weis",                "groceries"),
        ("lidl",                "groceries"),
        ("fresh market",        "groceries"),
        ("sprouts",             "groceries"),
        ("instacart",           "groceries"),
        // ── Fuel ─────────────────────────────────────────────────────────────
        ("sunoco",              "fuel"),
        ("1763 pf perry hall",  "fuel"),
        ("exxon",               "fuel"),
        ("mobil",               "fuel"),
        ("shell",               "fuel"),
        ("bp",                  "fuel"),
        ("chevron",             "fuel"),
        ("wawa",                "fuel"),
        ("sheetz",              "fuel"),
        ("7-eleven",            "fuel"),
        ("7eleven",             "fuel"),
        ("gas station",         "fuel"),
        ("fuel",                "fuel"),
        // ── Transport ────────────────────────────────────────────────────────
        ("uber *trip",          "transport"),
        ("uber* trip",          "transport"),
        ("lyft",                "transport"),
        ("metro",               "transport"),
        ("mta",                 "transport"),
        ("transit",             "transport"),
        ("parking",             "transport"),
        ("toll",                "transport"),
        ("zipcar",              "transport"),
        ("amtrak",              "transport"),
        ("greyhound",           "transport"),
        ("airline",             "transport"),
        ("united air",          "transport"),
        ("delta air",           "transport"),
        ("southwest air",       "transport"),
        ("american air",        "transport"),
        ("spirit air",          "transport"),
        // ── Shopping ─────────────────────────────────────────────────────────
        ("macys",               "shopping"),
        ("macy",                "shopping"),
        ("burlington",          "shopping"),
        ("amazon",              "shopping"),
        ("amzn",                "shopping"),
        ("ebay",                "shopping"),
        ("etsy",                "shopping"),
        ("bestbuy",             "shopping"),
        ("best buy",            "shopping"),
        ("apple store",         "shopping"),
        ("apple.com",           "shopping"),
        ("walmart.com",         "shopping"),
        ("target.com",          "shopping"),
        ("tjmaxx",              "shopping"),
        ("t.j. maxx",           "shopping"),
        ("marshalls",           "shopping"),
        ("ross ",               "shopping"),
        ("nordstrom",           "shopping"),
        ("h&m",                 "shopping"),
        ("zara",                "shopping"),
        ("gap",                 "shopping"),
        ("old navy",            "shopping"),
        ("forever 21",          "shopping"),
        ("nike",                "shopping"),
        ("adidas",              "shopping"),
        ("foot locker",         "shopping"),
        ("chewy",               "shopping"),
        ("wayfair",             "shopping"),
        // ── Entertainment ────────────────────────────────────────────────────
        ("bowlero",             "entertainment"),
        ("white marsh ice",     "entertainment"),
        ("netflix",             "entertainment"),
        ("hulu",                "entertainment"),
        ("disney",              "entertainment"),
        ("hbo",                 "entertainment"),
        ("spotify",             "entertainment"),
        ("apple music",         "entertainment"),
        ("youtube",             "entertainment"),
        ("twitch",              "entertainment"),
        ("steam",               "entertainment"),
        ("playstation",         "entertainment"),
        ("xbox",                "entertainment"),
        ("nintendo",            "entertainment"),
        ("amc",                 "entertainment"),
        ("regal",               "entertainment"),
        ("cinemark",            "entertainment"),
        ("movie",               "entertainment"),
        ("concert",             "entertainment"),
        ("ticketmaster",        "entertainment"),
        ("eventbrite",          "entertainment"),
        ("stubhub",             "entertainment"),
        // ── Recreation ───────────────────────────────────────────────────────
        ("gunpowder falls",     "recreation"),
        ("gym",                 "recreation"),
        ("planet fitness",      "recreation"),
        ("la fitness",          "recreation"),
        ("equinox",             "recreation"),
        ("crunch",              "recreation"),
        ("ymca",                "recreation"),
        ("peloton",             "recreation"),
        ("rei ",                "recreation"),
        ("dick's sporting",     "recreation"),
        ("dicks sporting",      "recreation"),
        ("golf",                "recreation"),
        ("tennis",              "recreation"),
        ("swimming",            "recreation"),
        // ── Education ────────────────────────────────────────────────────────
        ("ccbc essex",          "education"),
        ("ccbc",                "education"),
        ("university",          "education"),
        ("college",             "education"),
        ("coursera",            "education"),
        ("udemy",               "education"),
        ("chegg",               "education"),
        ("duolingo",            "education"),
        ("khan academy",        "education"),
        ("barnes & noble",      "education"),
        ("book",                "education"),
        // ── Bills & Utilities ────────────────────────────────────────────────
        ("mint mobile",         "bills"),
        ("at&t",                "bills"),
        ("verizon",             "bills"),
        ("t-mobile",            "bills"),
        ("tmobile",             "bills"),
        ("comcast",             "bills"),
        ("xfinity",             "bills"),
        ("cox ",                "bills"),
        ("spectrum",            "bills"),
        ("bge",                 "bills"),
        ("pepco",               "bills"),
        ("electric",            "bills"),
        ("utility",             "bills"),
        ("water bill",          "bills"),
        ("internet",            "bills"),
        ("rent",                "bills"),
        ("mortgage",            "bills"),
        // ── Insurance ────────────────────────────────────────────────────────
        ("iso student health",  "insurance"),
        ("geico",               "insurance"),
        ("progressive",         "insurance"),
        ("state farm",          "insurance"),
        ("allstate",            "insurance"),
        ("usaa",                "insurance"),
        ("cigna",               "insurance"),
        ("aetna",               "insurance"),
        ("bluecross",           "insurance"),
        ("blue cross",          "insurance"),
        ("insurance",           "insurance"),
        // ── Memberships & Subscriptions ──────────────────────────────────────
        ("bjs membership",      "memberships"),
        ("costco membership",   "memberships"),
        ("amazon prime",        "memberships"),
        ("prime video",         "memberships"),
        ("apple one",           "memberships"),
        ("icloud",              "memberships"),
        ("google one",          "memberships"),
        ("dropbox",             "memberships"),
        ("microsoft 365",       "memberships"),
        ("adobe",               "memberships"),
        ("patreon",             "memberships"),
        // ── Health & Pharmacy ────────────────────────────────────────────────
        ("cvs",                 "health"),
        ("walgreens",           "health"),
        ("rite aid",            "health"),
        ("pharmacy",            "health"),
        ("doctor",              "health"),
        ("dental",              "health"),
        ("vision",              "health"),
        ("hospital",            "health"),
        ("clinic",              "health"),
        ("urgent care",         "health"),
        ("lab corp",            "health"),
        ("quest diag",          "health"),
        // ── Automotive ───────────────────────────────────────────────────────
        ("auto diagnostic",     "automotive"),
        ("tims automotive",     "automotive"),
        ("jiffy lube",          "automotive"),
        ("midas",               "automotive"),
        ("valvoline",           "automotive"),
        ("autozone",            "automotive"),
        ("advance auto",        "automotive"),
        ("o'reilly",            "automotive"),
        ("car wash",            "automotive"),
        ("dmv",                 "automotive"),
        ("pep boys",            "automotive"),
        // ── Travel ───────────────────────────────────────────────────────────
        ("airbnb",              "travel"),
        ("hotel",               "travel"),
        ("marriott",            "travel"),
        ("hilton",              "travel"),
        ("hyatt",               "travel"),
        ("expedia",             "travel"),
        ("booking.com",         "travel"),
        ("vrbo",                "travel"),
        ("hertz",               "travel"),
        ("enterprise rent",     "travel"),
        ("avis",                "travel"),
    ];
    let desc = description.to_lowercase();
    for (keyword, category) in rules {
        if desc.contains(keyword) {
            return category.to_string();
        }
    }
    "Others".to_string()
}

// ── Claude (Anthropic) helpers ────────────────────────────────────────────────

async fn claude_api_call(prompt: String) -> Result<String, String> {
    let api_key = env::var("ANTHROPIC_API_KEY").map_err(|_| {
        let e = "ANTHROPIC_API_KEY not set — check your .env file".to_string();
        eprintln!("[Claude] {}", e);
        e
    })?;

    let body = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 2048,
        "messages": [{"role": "user", "content": prompt}]
    });

    let raw_response = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| { eprintln!("[Claude] HTTP error: {}", e); e.to_string() })?;

    let status = raw_response.status();
    let response: serde_json::Value = raw_response
        .json()
        .await
        .map_err(|e| { eprintln!("[Claude] Failed to parse response: {}", e); e.to_string() })?;

    if !status.is_success() {
        let err = format!("Claude API error {}: {}", status, response);
        eprintln!("[Claude] {}", err);
        return Err(err);
    }

    let text = response["content"][0]["text"]
        .as_str()
        .ok_or_else(|| {
            let err = format!("Unexpected Claude response shape: {}", response);
            eprintln!("[Claude] {}", err);
            err
        })?
        .to_string();

    eprintln!("[Claude] OK — {} chars returned", text.len());
    Ok(text)
}

/// Strip markdown code fences Claude sometimes wraps JSON in.
fn strip_code_fences(s: &str) -> &str {
    s.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

/// Use Claude to extract transactions from any bank statement text.
async fn claude_extract_transactions(text: &str) -> Result<Vec<Record>, String> {
    let truncated = &text[..text.len().min(20_000)];
    let prompt = format!(
        "You are a financial data extractor. From the bank/credit card statement below, \
         extract every purchase/expense transaction.\n\
         Return ONLY a JSON array. Each object must have exactly:\n\
         - \"date\": string in MM/DD/YYYY format\n\
         - \"description\": merchant name in UPPERCASE\n\
         - \"amount\": positive number (dollars, no $ sign)\n\
         Exclude payments, refunds, and credits. Return [] if none found.\n\
         No markdown, no explanation — only the JSON array.\n\n\
         Statement:\n{}",
        truncated
    );

    match claude_api_call(prompt).await {
        Ok(raw) => {
            let cleaned = strip_code_fences(&raw);
            let records = serde_json::from_str::<Vec<serde_json::Value>>(cleaned)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| {
                    let date = v["date"].as_str()?.to_string();
                    let description = v["description"].as_str()?.to_string();
                    let amount = v["amount"].as_f64()
                        .or_else(|| {
                            v["amount"].as_str()
                                .and_then(|s| s.replace('$', "").replace(',', "").parse().ok())
                        })?;
                    if amount <= 0.0 { return None; }
                    let category = categorize(&description);
                    Some(Record { date, description, amount, category })
                })
                .collect();
            Ok(records)
        }
        Err(e) => Err(e),
    }
}

// ── PDF parsing ───────────────────────────────────────────────────────────────

fn read_pdf(file_path: &str) -> Result<Vec<Record>, Box<dyn std::error::Error>> {
    let text = pdf_extract::extract_text(file_path)?;
    let mut records = Vec::new();
    for cap in RE.captures_iter(&text) {
        let date = cap[1].to_string();
        let description = cap[2].trim().to_string();
        let amount: f64 = cap[3].replace(",", "").parse()?;
        let category = categorize(&description);
        records.push(Record { date, amount, description, category });
    }
    Ok(records)
}

fn read_csv(file_path: &str) -> Result<Vec<Record>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_path(file_path)?;
    let mut records = Vec::new();
    for result in reader.deserialize() {
        let mut record: Record = result?;
        if record.category.is_empty() {
            record.category = categorize(&record.description);
        }
        records.push(record);
    }
    Ok(records)
}

/// Try known bank regexes first; fall back to Gemini for any other format.
async fn extract_records(file_path: &str) -> Result<Vec<Record>, String> {
    let text = pdf_extract::extract_text(file_path).map_err(|e| e.to_string())?;

    // Fast path 1: Apple Card regex
    let apple_records: Vec<Record> = RE
        .captures_iter(&text)
        .filter_map(|cap| {
            let amount: f64 = cap[3].replace(",", "").parse().ok()?;
            if amount <= 0.0 { return None; }
            let description = cap[2].trim().to_string();
            let category = categorize(&description);
            Some(Record { date: cap[1].to_string(), amount, description, category })
        })
        .collect();

    if !apple_records.is_empty() {
        return Ok(apple_records);
    }

    // Fast path 2: Discover Account Activity regex
    let discover_records: Vec<Record> = RE_DISCOVER
        .captures_iter(&text)
        .filter_map(|cap| {
            let amount: f64 = cap[3].replace(",", "").parse().ok()?;
            if amount <= 0.0 { return None; }
            // Collapse multi-line descriptions into a single string
            let description = cap[2].split_whitespace().collect::<Vec<_>>().join(" ");
            let category = categorize(&description);
            let date = discover_date(&cap[1]);
            Some(Record { date, amount, description, category })
        })
        .collect();

    if !discover_records.is_empty() {
        return Ok(discover_records);
    }

    // Slow path: ask Claude to understand any other bank format
    eprintln!("[PDF] No regex matched. Extracted text (first 500 chars):\n{}", &text[..text.len().min(500)]);
    claude_extract_transactions(&text).await
}

// ── Teller helpers ────────────────────────────────────────────────────────────

const TELLER_API: &str = "https://api.teller.io";

/// Builds an HTTP client with optional mTLS client certificate.
/// Sandbox works without certs. Development/Production require them.
/// Set TELLER_CERT_PATH and TELLER_KEY_PATH in .env for non-sandbox use.
fn teller_http_client() -> reqwest::Client {
    let cert_path = env::var("TELLER_CERT_PATH").ok();
    let key_path  = env::var("TELLER_KEY_PATH").ok();

    let mut builder = reqwest::Client::builder();

    if let (Some(cp), Some(kp)) = (cert_path, key_path) {
        match (std::fs::read(&cp), std::fs::read(&kp)) {
            (Ok(mut cert_pem), Ok(key_pem)) => {
                // reqwest::Identity::from_pem expects cert + key concatenated
                cert_pem.push(b'\n');
                cert_pem.extend_from_slice(&key_pem);
                match reqwest::Identity::from_pem(&cert_pem) {
                    Ok(identity) => { builder = builder.identity(identity); }
                    Err(e) => eprintln!("[Teller] Failed to load client cert: {}", e),
                }
            }
            _ => eprintln!("[Teller] Could not read TELLER_CERT_PATH / TELLER_KEY_PATH"),
        }
    }

    builder.build().unwrap_or_default()
}

/// GET /accounts — list all accounts for this enrollment's access token.
async fn teller_get_accounts_api(access_token: &str) -> Result<Vec<serde_json::Value>, String> {
    let resp = teller_http_client()
        .get(format!("{}/accounts", TELLER_API))
        .basic_auth(access_token, Some(""))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    if !status.is_success() {
        return Err(format!("Teller accounts error {}: {}", status, data));
    }

    Ok(data.as_array().cloned().unwrap_or_default())
}

/// GET /accounts/{id}/transactions — paginated fetch stopping at `since_id`.
/// Returns (records, newest_transaction_id_seen).
/// Teller convention: negative amount = debit (expense), positive = credit (skip).
async fn teller_fetch_transactions(
    access_token: &str,
    account_id: &str,
    since_id: Option<&str>,
) -> Result<(Vec<Record>, Option<String>), String> {
    let http = teller_http_client();
    let mut all_records: Vec<Record> = Vec::new();
    let mut from_id: Option<String> = None; // pagination cursor (oldest ID on last page)
    let mut newest_id: Option<String> = None; // newest transaction seen this sync

    loop {
        let mut req = http
            .get(format!("{}/accounts/{}/transactions", TELLER_API, account_id))
            .basic_auth(access_token, Some(""))
            .query(&[("count", "100")]);

        if let Some(ref fid) = from_id {
            req = req.query(&[("from_id", fid.as_str())]);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

        if !status.is_success() {
            return Err(format!("Teller transactions error {}: {}", status, data));
        }

        let page = match data.as_array() {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };

        let page_len = page.len();
        let mut oldest_id_this_page: Option<String> = None;
        let mut stop = false;

        for txn in &page {
            let id = txn["id"].as_str().unwrap_or("").to_string();

            // Track the newest transaction ID (first on first page)
            if newest_id.is_none() {
                newest_id = Some(id.clone());
            }

            // Stop if we've reached the last transaction we already have
            if since_id.map(|s| s == id).unwrap_or(false) {
                stop = true;
                break;
            }

            oldest_id_this_page = Some(id.clone());

            // Teller: negative = debit/expense, positive = credit/income — skip credits
            let amount: f64 = txn["amount"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            if amount >= 0.0 { continue; }
            let amount = amount.abs();

            // Date: YYYY-MM-DD → MM/DD/YYYY
            let date = match txn["date"].as_str() {
                Some(d) => {
                    let p: Vec<&str> = d.split('-').collect();
                    if p.len() == 3 { format!("{}/{}/{}", p[1], p[2], p[0]) }
                    else { d.to_string() }
                }
                None => continue,
            };

            let description = txn["description"]
                .as_str()
                .unwrap_or("Unknown")
                .to_uppercase();
            let category = categorize(&description);
            all_records.push(Record { date, amount, description, category });
        }

        if stop || page_len < 100 {
            break; // caught up or no more pages
        }

        // from_id makes Teller return transactions OLDER than that ID
        from_id = oldest_id_this_page;
        if from_id.is_none() { break; }
    }

    Ok((all_records, newest_id))
}

// ── Teller handlers ───────────────────────────────────────────────────────────

/// POST /teller/enroll
/// Body: { "access_token": "...", "enrollment_id": "...", "institution_name": "Chase" }
///
/// Called by the frontend after the user completes Teller Connect. The frontend
/// receives these values in the onSuccess callback and sends them here.
/// The server then fetches and stores the user's bank accounts from Teller.
async fn teller_enroll(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(body): Json<TellerEnrollRequest>,
) -> impl IntoResponse {
    // Fetch accounts from Teller (outside DB lock)
    let accounts = match teller_get_accounts_api(&body.access_token).await {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))),
    };

    let conn = state.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();

    let enrollment_row_id = match conn.execute(
        "INSERT INTO teller_enrollments
         (user_id, access_token, enrollment_id, institution_name, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![auth.user_id, body.access_token, body.enrollment_id, body.institution_name, now],
    ) {
        Ok(_) => conn.last_insert_rowid(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    };

    let mut account_count = 0;
    for acct in &accounts {
        let account_id  = acct["id"].as_str().unwrap_or("");
        let name        = acct["name"].as_str().unwrap_or("Account");
        let acct_type   = acct["type"].as_str().unwrap_or("unknown");
        let last_four   = acct["last_four"].as_str();

        let _ = conn.execute(
            "INSERT OR IGNORE INTO teller_accounts
             (teller_enrollment_id, user_id, account_id, name, account_type, last_four)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![enrollment_row_id, auth.user_id, account_id, name, acct_type, last_four],
        );
        account_count += 1;
    }

    (StatusCode::CREATED, Json(json!({
        "message": format!(
            "Connected {} with {} account(s). Run POST /teller/sync to import transactions.",
            body.institution_name, account_count
        ),
        "accounts": account_count
    })))
}

/// POST /teller/sync
/// Fetches new transactions from every connected account and saves them.
/// Uses each account's last_transaction_id as a cursor so only new data is pulled.
async fn teller_sync(
    auth: AuthUser,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Collect accounts without holding the lock during async Teller calls
    // (id, access_token, account_id, last_transaction_id, institution_name)
    let accounts: Vec<(i64, String, String, Option<String>, String)> = {
        let conn = state.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT ta.id, te.access_token, ta.account_id,
                    NULLIF(ta.last_transaction_id, ''), te.institution_name
             FROM teller_accounts ta
             JOIN teller_enrollments te ON ta.teller_enrollment_id = te.id
             WHERE ta.user_id = ?1",
        ) {
            Ok(s) => s,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
        };
        stmt.query_map(params![auth.user_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    };

    if accounts.is_empty() {
        return (StatusCode::OK, Json(json!({
            "message": "No connected banks. Use POST /teller/enroll to connect one first.",
            "imported": 0
        })));
    }

    let mut total_imported = 0;

    for (acct_row_id, access_token, teller_account_id, last_id, institution_name) in accounts {
        match teller_fetch_transactions(&access_token, &teller_account_id, last_id.as_deref()).await {
            Ok((records, newest_id)) => {
                let count = records.len();
                let conn = state.lock().unwrap();
                let _ = save_records(&conn, &records, Some(auth.user_id));
                // Advance cursor to the newest transaction seen
                if let Some(ref nid) = newest_id {
                    let _ = conn.execute(
                        "UPDATE teller_accounts SET last_transaction_id = ?1 WHERE id = ?2",
                        params![nid, acct_row_id],
                    );
                }
                total_imported += count;
                eprintln!("[Teller] Synced {} transactions from {}", count, institution_name);
            }
            Err(e) => eprintln!("[Teller] Sync error for {}: {}", institution_name, e),
        }
    }

    (StatusCode::OK, Json(json!({
        "message": format!("Synced {} new transaction(s) from connected banks", total_imported),
        "imported": total_imported
    })))
}

/// GET /teller/accounts — list all connected bank accounts.
async fn get_teller_accounts(auth: AuthUser, State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.lock().unwrap();
    let mut stmt = match conn.prepare(
        "SELECT ta.id, te.institution_name, ta.name, ta.account_type, ta.last_four
         FROM teller_accounts ta
         JOIN teller_enrollments te ON ta.teller_enrollment_id = te.id
         WHERE ta.user_id = ?1
         ORDER BY te.institution_name, ta.name",
    ) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    };

    let accounts: Vec<TellerAccountRow> = stmt
        .query_map(params![auth.user_id], |row| {
            Ok(TellerAccountRow {
                id:               row.get(0)?,
                institution_name: row.get(1)?,
                account_name:     row.get(2)?,
                account_type:     row.get(3)?,
                last_four:        row.get(4)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    (StatusCode::OK, Json(json!({ "accounts": accounts })))
}

/// DELETE /teller/accounts/{id} — disconnect a bank account.
/// Removes the parent enrollment if it has no remaining accounts.
async fn teller_disconnect(
    auth: AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(account_id): axum::extract::Path<i64>,
) -> impl IntoResponse {
    let conn = state.lock().unwrap();

    let enrollment_id: i64 = match conn.query_row(
        "SELECT teller_enrollment_id FROM teller_accounts WHERE id = ?1 AND user_id = ?2",
        params![account_id, auth.user_id],
        |row| row.get(0),
    ) {
        Ok(id) => id,
        Err(_) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "Account not found" }))),
    };

    let _ = conn.execute(
        "DELETE FROM teller_accounts WHERE id = ?1 AND user_id = ?2",
        params![account_id, auth.user_id],
    );

    // Remove the enrollment if no accounts remain under it
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM teller_accounts WHERE teller_enrollment_id = ?1",
            params![enrollment_id],
            |row| row.get(0),
        )
        .unwrap_or(1);

    if remaining == 0 {
        let _ = conn.execute(
            "DELETE FROM teller_enrollments WHERE id = ?1 AND user_id = ?2",
            params![enrollment_id, auth.user_id],
        );
    }

    (StatusCode::OK, Json(json!({ "message": "Bank account disconnected" })))
}

// ── Database setup ────────────────────────────────────────────────────────────

fn create_db() -> Result<Connection, Box<dyn std::error::Error>> {
    let conn = Connection::open("transactions.db")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS users (
            id            INTEGER PRIMARY KEY,
            email         TEXT UNIQUE NOT NULL,
            password_hash TEXT NOT NULL,
            created_at    TEXT NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS transactions (
            id          INTEGER PRIMARY KEY,
            date        TEXT NOT NULL,
            amount      REAL NOT NULL,
            description TEXT NOT NULL,
            category    TEXT NOT NULL DEFAULT '',
            user_id     INTEGER REFERENCES users(id),
            UNIQUE(date, description, amount, user_id)
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS cash_entries (
            id          INTEGER PRIMARY KEY,
            user_id     INTEGER NOT NULL REFERENCES users(id),
            date        TEXT NOT NULL,
            amount      REAL NOT NULL,
            entry_type  TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT ''
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS teller_enrollments (
            id               INTEGER PRIMARY KEY,
            user_id          INTEGER NOT NULL REFERENCES users(id),
            access_token     TEXT NOT NULL,
            enrollment_id    TEXT NOT NULL,
            institution_name TEXT NOT NULL,
            created_at       TEXT NOT NULL
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS teller_accounts (
            id                    INTEGER PRIMARY KEY,
            teller_enrollment_id  INTEGER NOT NULL REFERENCES teller_enrollments(id),
            user_id               INTEGER NOT NULL REFERENCES users(id),
            account_id            TEXT NOT NULL,
            name                  TEXT NOT NULL,
            account_type          TEXT NOT NULL,
            last_four             TEXT,
            last_transaction_id   TEXT,
            UNIQUE(account_id, user_id)
        )",
        [],
    )?;

    // Add new user profile columns (safe to call repeatedly — errors ignored)
    let _ = conn.execute("ALTER TABLE users ADD COLUMN phone TEXT", []);
    let _ = conn.execute("ALTER TABLE users ADD COLUMN security_question TEXT", []);
    let _ = conn.execute("ALTER TABLE users ADD COLUMN security_answer TEXT", []);

    // Detect and fix the old UNIQUE(date, description, amount) constraint
    // that excludes user_id — causes INSERT OR IGNORE to silently discard uploads.
    let unique_index_names: Vec<String> = {
        let mut s = conn.prepare(
            "SELECT name FROM pragma_index_list('transactions') WHERE \"unique\" = 1",
        )?;
        s.query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect()
    };

    let unique_covers_user_id = unique_index_names.iter().any(|idx| {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM pragma_index_info('{}') WHERE name = 'user_id'",
                idx
            ),
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    });

    if !unique_covers_user_id {
        conn.execute_batch(
            "BEGIN;
             DROP TABLE IF EXISTS transactions_old;
             ALTER TABLE transactions RENAME TO transactions_old;
             CREATE TABLE transactions (
                 id          INTEGER PRIMARY KEY,
                 date        TEXT NOT NULL,
                 amount      REAL NOT NULL,
                 description TEXT NOT NULL,
                 category    TEXT NOT NULL DEFAULT '',
                 user_id     INTEGER REFERENCES users(id),
                 UNIQUE(date, description, amount, user_id)
             );
             INSERT OR IGNORE INTO transactions (id, date, amount, description, category)
                 SELECT id, date, amount, description, category FROM transactions_old;
             DROP TABLE transactions_old;
             COMMIT;",
        )?;
    }

    Ok(conn)
}

// ── Core data functions ───────────────────────────────────────────────────────

fn save_records(
    conn: &Connection,
    records: &[Record],
    user_id: Option<i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    for record in records {
        conn.execute(
            "INSERT OR IGNORE INTO transactions (date, amount, description, category, user_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![record.date, record.amount, record.description, record.category, user_id],
        )?;
    }
    Ok(())
}

fn build_summary(conn: &Connection, user_id: i64) -> Result<String, Box<dyn std::error::Error>> {
    let mut summary = String::new();
    summary.push_str("Spending Summary: \n\n");

    let (total_count, total_amount): (i64, f64) = conn.query_row(
        "SELECT COUNT(*), ROUND(COALESCE(SUM(amount), 0.0), 2)
         FROM transactions
         WHERE category != 'payment' AND user_id = ?1",
        params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    if total_count == 0 {
        return Ok("No transactions found. Upload a statement to get started.".to_string());
    }

    summary.push_str(&format!(
        "Total Transactions: {}   Total Spent: ${:.2}\n\n",
        total_count, total_amount
    ));

    summary.push_str("Spending by month:\n");
    let mut stmt = conn.prepare(
        "SELECT substr(date, 7, 4) || '-' || substr(date, 1, 2) as month,
                COUNT(*) as count, ROUND(SUM(amount), 2) as total
         FROM transactions
         WHERE category != 'payment' AND user_id = ?1
         GROUP BY month ORDER BY month ASC",
    )?;
    for row in stmt.query_map(params![user_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, f64>(2)?))
    })? {
        let (month, count, total) = row?;
        summary.push_str(&format!("  {}: {} transactions  ${:.2}\n", month, count, total));
    }

    summary.push_str("\nSpending by category:\n");
    // FIX: ?1 appears twice in this SQL (outer WHERE + subquery) — only ONE binding needed.
    let mut stmt = conn.prepare(
        "SELECT category,
                COUNT(*) as count,
                ROUND(SUM(amount), 2) as total,
                ROUND(SUM(amount) * 100.0 / (
                    SELECT COALESCE(SUM(amount), 1) FROM transactions
                    WHERE category != 'payment' AND user_id = ?1
                ), 1) as pct
         FROM transactions
         WHERE category != 'payment' AND user_id = ?1
         GROUP BY category ORDER BY total DESC",
    )?;
    for row in stmt.query_map(params![user_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })? {
        let (cat, count, total, pct) = row?;
        summary.push_str(&format!(
            "  {:<20} {:>3} transactions  ${:>8.2}  ({:.1}%)\n",
            cat, count, total, pct
        ));
    }

    summary.push_str("\nTop 5 merchants:\n");
    let mut stmt = conn.prepare(
        "SELECT description, COUNT(*) as visits, ROUND(SUM(amount), 2) as total
         FROM transactions
         WHERE category != 'payment' AND user_id = ?1
         GROUP BY description ORDER BY total DESC LIMIT 5",
    )?;
    for row in stmt.query_map(params![user_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, f64>(2)?))
    })? {
        let (desc, visits, total) = row?;
        let short = if desc.len() > 40 { &desc[..40] } else { &desc };
        summary.push_str(&format!("  {:<40}  {}x  ${:.2}\n", short, visits, total));
    }

    summary.push_str("\nBiggest transactions:\n");
    let mut stmt = conn.prepare(
        "SELECT date, description, amount, category
         FROM transactions
         WHERE category != 'payment' AND user_id = ?1
         ORDER BY amount DESC LIMIT 5",
    )?;
    for row in stmt.query_map(params![user_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })? {
        let (date, desc, amount, cat) = row?;
        let short = if desc.len() > 35 { &desc[..35] } else { &desc };
        summary.push_str(&format!(
            "  {}  ${:>8.2}  {:<15}  {}\n",
            date, amount, cat, short
        ));
    }

    Ok(summary)
}

// ── Debug endpoint ────────────────────────────────────────────────────────────

async fn test_claude() -> impl IntoResponse {
    let key = env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| "NOT SET".to_string());
    let key_preview = if key == "NOT SET" {
        "NOT SET".to_string()
    } else {
        format!("{}...{}", &key[..4.min(key.len())], &key[key.len().saturating_sub(4)..])
    };

    let status = match claude_api_call("Say 'ok' and nothing else.".to_string()).await {
        Ok(_) => "reachable".to_string(),
        Err(e) => format!("error: {}", e),
    };

    (StatusCode::OK, Json(json!({
        "key_preview": key_preview,
        "status": status,
    })))
}

// ── Auth handlers ─────────────────────────────────────────────────────────────

async fn register(
    State(state): State<AppState>,
    Json(body): Json<AuthRequest>,
) -> impl IntoResponse {
    let password_hash = match hash(&body.password, DEFAULT_COST) {
        Ok(h) => h,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to hash password"}))),
    };

    let conn = state.lock().unwrap();
    let now = chrono::Utc::now().to_rfc3339();

    match conn.execute(
        "INSERT INTO users (email, password_hash, created_at) VALUES (?1, ?2, ?3)",
        params![body.email, password_hash, now],
    ) {
        Ok(_) => {
            let user_id = conn.last_insert_rowid();
            match create_token(user_id) {
                Ok(token) => (StatusCode::CREATED, Json(json!({"token": token}))),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to create token"}))),
            }
        }
        Err(e) => {
            if e.to_string().contains("UNIQUE") {
                (StatusCode::CONFLICT, Json(json!({"error": "Email already registered"})))
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            }
        }
    }
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<AuthRequest>,
) -> impl IntoResponse {
    let conn = state.lock().unwrap();
    let result = conn.query_row(
        "SELECT id, password_hash FROM users WHERE email = ?1",
        params![body.email],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    );
    match result {
        Ok((user_id, stored_hash)) => match verify(&body.password, &stored_hash) {
            Ok(true) => match create_token(user_id) {
                Ok(token) => (StatusCode::OK, Json(json!({"token": token}))),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to create token"}))),
            },
            _ => (StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid credentials"}))),
        },
        Err(_) => (StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid credentials"}))),
    }
}

// ── Protected handlers ────────────────────────────────────────────────────────

async fn get_summary(auth: AuthUser, State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.lock().unwrap();
    match build_summary(&conn, auth.user_id) {
        Ok(summary) => (StatusCode::OK, Json(json!({"summary": summary}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn get_transactions(auth: AuthUser, State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.lock().unwrap();
    let mut stmt = match conn.prepare(
        "SELECT date, description, amount, category FROM transactions
         WHERE user_id = ?1 ORDER BY date DESC",
    ) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    };

    let records: Vec<Record> = stmt
        .query_map(params![auth.user_id], |row| {
            Ok(Record {
                date: row.get(0)?,
                description: row.get(1)?,
                amount: row.get(2)?,
                category: row.get(3)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    (StatusCode::OK, Json(json!({"transactions": records})))
}

/// Upload a PDF statement. Tries Apple Card regex first; falls back to Gemini
/// so any bank's PDF format is supported. Lock is NOT held during the async
/// PDF/Gemini processing — only acquired for the final DB write.
async fn upload(
    auth: AuthUser,
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // ── Phase 1: read file bytes (no lock) ───────────────────────────────────
    let field = match multipart.next_field().await.unwrap_or(None) {
        Some(f) => f,
        None => return (StatusCode::BAD_REQUEST, Json(json!({"error": "No file uploaded"}))),
    };

    let file_name = field.file_name().unwrap_or("upload.pdf").to_string();
    let data = match field.bytes().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))),
    };

    let temp_path = format!("/tmp/{}", file_name);
    if let Err(e) = std::fs::write(&temp_path, &data) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})));
    }

    // ── Phase 2: extract records — may call Gemini (no lock held) ───────────
    let records = match extract_records(&temp_path).await {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error": e})));
        }
    };

    let count = records.len();
    let _ = std::fs::remove_file(&temp_path);

    // ── Phase 3: save to DB (hold lock briefly) ───────────────────────────────
    let save_result = {
        let conn = state.lock().unwrap();
        save_records(&conn, &records, Some(auth.user_id))
    };
    if let Err(e) = save_result {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})));
    }

    let message = if count == 0 {
        format!(
            "No transactions found in {}. If this isn't an Apple Card or Discover statement, \
             make sure ANTHROPIC_API_KEY is set — Claude is used to parse other formats.",
            file_name
        )
    } else {
        format!("Successfully processed {} transactions from {}", count, file_name)
    };

    (StatusCode::OK, Json(json!({"message": message, "count": count})))
}

/// GET /analytics — monthly average, top expenses, AI tips.
/// Collects DB data first (lock held briefly), then calls Gemini (no lock).
async fn get_analytics(auth: AuthUser, State(state): State<AppState>) -> impl IntoResponse {
    // ── Phase 1: query DB ─────────────────────────────────────────────────────
    let (monthly_avg, top_expenses, _spending_text) = {
        let conn = state.lock().unwrap();

        let monthly_avg: f64 = conn
            .query_row(
                "SELECT COALESCE(AVG(monthly_total), 0.0) FROM (
                     SELECT SUM(amount) as monthly_total
                     FROM transactions
                     WHERE category != 'payment' AND user_id = ?1
                     GROUP BY substr(date, 7, 4) || '-' || substr(date, 1, 2)
                 )",
                params![auth.user_id],
                |row| row.get(0),
            )
            .unwrap_or(0.0);

        let mut stmt = conn
            .prepare(
                "SELECT date, description, amount, category
                 FROM transactions
                 WHERE category != 'payment' AND user_id = ?1
                 ORDER BY amount DESC LIMIT 5",
            )
            .unwrap();

        let top_expenses: Vec<TopExpense> = stmt
            .query_map(params![auth.user_id], |row| {
                Ok(TopExpense {
                    date: row.get(0)?,
                    description: row.get(1)?,
                    amount: row.get(2)?,
                    category: row.get(3)?,
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let mut stmt = conn
            .prepare(
                "SELECT category, ROUND(SUM(amount), 2) as total
                 FROM transactions
                 WHERE category != 'payment' AND user_id = ?1
                 GROUP BY category ORDER BY total DESC",
            )
            .unwrap();

        let category_lines: Vec<String> = stmt
            .query_map(params![auth.user_id], |row| {
                Ok(format!(
                    "{}: ${:.2}",
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let spending_text = format!(
            "Monthly average spend: ${:.2}\nSpending by category:\n{}",
            monthly_avg,
            category_lines.join("\n")
        );

        (monthly_avg, top_expenses, spending_text)
        // lock released here
    };

    (
        StatusCode::OK,
        Json(json!({
            "monthly_average": monthly_avg,
            "top_expenses": top_expenses,
        })),
    )
}

// ── AI chat handler ───────────────────────────────────────────────────────────

async fn chat(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(body): Json<ChatRequest>,
) -> impl IntoResponse {
    let context = {
        let conn = state.lock().unwrap();
        build_summary(&conn, auth.user_id).unwrap_or_else(|_| "No transaction data available.".to_string())
    };

    let prompt = format!(
        "You are a personal finance assistant. Here is the user's spending summary:\n{}\n\n\
         Answer the user's question concisely and helpfully. \
         Do not add any prefix or label to your response.\n\n\
         User: {}",
        context, body.message
    );

    match claude_api_call(prompt).await {
        Ok(reply) => (StatusCode::OK, Json(json!({"reply": reply}))),
        Err(e)    => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
    }
}

// ── Account handlers ──────────────────────────────────────────────────────────

async fn get_account(auth: AuthUser, State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.lock().unwrap();
    match conn.query_row(
        "SELECT email, phone, security_question FROM users WHERE id = ?1",
        params![auth.user_id],
        |row| Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        )),
    ) {
        Ok((email, phone, security_question)) => (
            StatusCode::OK,
            Json(json!({ "email": email, "phone": phone, "security_question": security_question })),
        ),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn update_account(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(body): Json<AccountUpdateRequest>,
) -> impl IntoResponse {
    let conn = state.lock().unwrap();

    if let Some(ref phone) = body.phone {
        if let Err(e) = conn.execute(
            "UPDATE users SET phone = ?1 WHERE id = ?2",
            params![phone, auth.user_id],
        ) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})));
        }
    }

    if let (Some(ref question), Some(ref answer)) = (body.security_question, body.security_answer) {
        let answer_hash = match hash(answer, DEFAULT_COST) {
            Ok(h) => h,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to hash answer"}))),
        };
        if let Err(e) = conn.execute(
            "UPDATE users SET security_question = ?1, security_answer = ?2 WHERE id = ?3",
            params![question, answer_hash, auth.user_id],
        ) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})));
        }
    }

    (StatusCode::OK, Json(json!({"message": "Account updated"})))
}

async fn change_password(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(body): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    let conn = state.lock().unwrap();

    let current_hash: String = match conn.query_row(
        "SELECT password_hash FROM users WHERE id = ?1",
        params![auth.user_id],
        |row| row.get(0),
    ) {
        Ok(h) => h,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    };

    match verify(&body.current_password, &current_hash) {
        Ok(true)  => {}
        Ok(false) => return (StatusCode::UNAUTHORIZED, Json(json!({"error": "Current password is incorrect"}))),
        Err(_)    => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Password verification failed"}))),
    }

    let new_hash = match hash(&body.new_password, DEFAULT_COST) {
        Ok(h) => h,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to hash password"}))),
    };

    match conn.execute(
        "UPDATE users SET password_hash = ?1 WHERE id = ?2",
        params![new_hash, auth.user_id],
    ) {
        Ok(_)  => (StatusCode::OK, Json(json!({"message": "Password changed successfully"}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

// ── Cash entry handlers ───────────────────────────────────────────────────────

async fn add_cash_entry(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(body): Json<CashEntryRequest>,
) -> impl IntoResponse {
    if body.entry_type != "income" && body.entry_type != "expense" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "entry_type must be 'income' or 'expense'"})),
        );
    }
    if body.amount <= 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "amount must be positive"})),
        );
    }

    let conn = state.lock().unwrap();
    match conn.execute(
        "INSERT INTO cash_entries (user_id, date, amount, entry_type, description)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![auth.user_id, body.date, body.amount, body.entry_type, body.description],
    ) {
        Ok(_) => (StatusCode::CREATED, Json(json!({"message": "Cash entry added"}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn get_cash_entries(auth: AuthUser, State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.lock().unwrap();

    let mut stmt = match conn.prepare(
        "SELECT id, date, amount, entry_type, description
         FROM cash_entries WHERE user_id = ?1 ORDER BY date DESC",
    ) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    };

    let entries: Vec<CashEntry> = stmt
        .query_map(params![auth.user_id], |row| {
            Ok(CashEntry {
                id: row.get(0)?,
                date: row.get(1)?,
                amount: row.get(2)?,
                entry_type: row.get(3)?,
                description: row.get(4)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    let total_income: f64 = entries.iter().filter(|e| e.entry_type == "income").map(|e| e.amount).sum();
    let total_expense: f64 = entries.iter().filter(|e| e.entry_type == "expense").map(|e| e.amount).sum();

    (
        StatusCode::OK,
        Json(json!({
            "entries": entries,
            "total_income": total_income,
            "total_expense": total_expense,
            "net": total_income - total_expense,
        })),
    )
}

async fn delete_cash_entry(
    auth: AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> impl IntoResponse {
    let conn = state.lock().unwrap();
    match conn.execute(
        "DELETE FROM cash_entries WHERE id = ?1 AND user_id = ?2",
        params![id, auth.user_id],
    ) {
        Ok(0) => (StatusCode::NOT_FOUND, Json(json!({"error": "Entry not found"}))),
        Ok(_) => (StatusCode::OK, Json(json!({"message": "Deleted"}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok(); // load .env if present
    let conn = create_db()?;

    let args: Vec<String> = env::args().collect();
    let pdf_files = &args[1..];

    if !pdf_files.is_empty() {
        let mut total = 0;
        for path in pdf_files {
            match read_pdf(path) {
                Ok(records) => {
                    let count = records.len();
                    save_records(&conn, &records, None)?;
                    println!(" {}: {} transactions saved.", path, count);
                    total += count;
                }
                Err(e) => println!("Error processing {}: {}", path, e),
            }
        }
        println!("Total {} transactions saved.", total);
    }

    let state = Arc::new(Mutex::new(conn));

    let app = Router::new()
        .route("/health",       get(health))
        .route("/test-claude",  get(test_claude))
        .route("/register",     post(register))
        .route("/login",        post(login))
        .route("/summary",      get(get_summary))
        .route("/transactions", get(get_transactions))
        .route("/upload",       post(upload))
        .route("/analytics",    get(get_analytics))
        .route("/chat",              post(chat))
        .route("/account",           get(get_account).put(update_account))
        .route("/account/password",  axum::routing::put(change_password))
        .route("/cash",              post(add_cash_entry).get(get_cash_entries))
        .route("/cash/{id}",         axum::routing::delete(delete_cash_entry))
        // Teller bank connection
        .route("/teller/enroll",       post(teller_enroll))
        .route("/teller/sync",         post(teller_sync))
        .route("/teller/accounts",     get(get_teller_accounts))
        .route("/teller/accounts/{id}", axum::routing::delete(teller_disconnect))
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = "0.0.0.0:3000";
    println!("FINAD API running on {}", addr);
    println!("  POST /register | POST /login");
    println!("  GET  /summary | GET /transactions | GET /analytics  (Bearer token)");
    println!("  POST /upload | POST /cash | GET /cash | DELETE /cash/:id  (Bearer token)");
    println!("  --- Bank connection (Teller) ---");
    println!("  POST /teller/enroll        → store enrollment after Teller Connect flow");
    println!("  POST /teller/sync          → pull latest transactions from connected banks");
    println!("  GET  /teller/accounts      → list connected bank accounts");
    println!("  DELETE /teller/accounts/:id → disconnect a bank account");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
