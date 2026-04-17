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

static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(\d{2}/\d{2}/\d{4})\s+([A-Z0-9 \*\.\-]+?)\s+\d+%\s+\$[\d\.]+\s+\$(-?[\d\.]+)",
    )
    .unwrap()
});

fn categorize(description: &str) -> String {
    let rules: &[(&str, &str)] = &[
        ("akira ramen",         "dining"),
        ("swadesh",             "dining"),
        ("kong",                "dining"),
        ("tst*",                "dining"),
        ("potomac pizza",       "dining"),
        ("lao sze chuan",       "dining"),
        ("starbucks",           "dining"),
        ("mcdonalds",           "dining"),
        ("bakery momo",         "dining"),
        ("tous les jours",      "dining"),
        ("spice",               "dining"),
        ("py *kung fu tea",     "dining"),
        ("uber *trip",          "transport"),
        ("sunoco",              "fuel"),
        ("gas",                 "fuel"),
        ("1763 pf perry hall",  "fuel"),
        ("h mart",              "groceries"),
        ("safeway",             "groceries"),
        ("walmart",             "groceries"),
        ("macys",               "shopping"),
        ("burlington",          "shopping"),
        ("bowlero",             "entertainment"),
        ("white marsh ice",     "entertainment"),
        ("gunpowder falls",     "recreation"),
        ("ccbc essex",          "education"),
        ("auto diagnostic",     "automotive"),
        ("tims automotive",     "automotive"),
        ("mint mobile",         "bills"),
        ("iso student health",  "insurance"),
        ("bjs membership",      "memberships"),
    ];
    let desc = description.to_lowercase();
    for (keyword, category) in rules {
        if desc.contains(keyword) {
            return category.to_string();
        }
    }
    "Others".to_string()
}

// ── Gemini helpers ────────────────────────────────────────────────────────────

async fn gemini_api_call(prompt: String) -> Result<String, String> {
    let api_key = env::var("GEMINI_API_KEY")
        .map_err(|_| "GEMINI_API_KEY not set".to_string())?;

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key={}",
        api_key
    );

    let body = json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"temperature": 0.1}
    });

    let response: serde_json::Value = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    response["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .ok_or_else(|| "Empty Gemini response".to_string())
        .map(|s| s.to_string())
}

/// Strip markdown code fences Gemini sometimes wraps JSON in.
fn strip_code_fences(s: &str) -> &str {
    s.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

/// Use Gemini to extract transactions from any bank statement text.
async fn gemini_extract_transactions(text: &str) -> Vec<Record> {
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

    match gemini_api_call(prompt).await {
        Ok(raw) => {
            let cleaned = strip_code_fences(&raw);
            serde_json::from_str::<Vec<serde_json::Value>>(cleaned)
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
                .collect()
        }
        Err(_) => Vec::new(),
    }
}

/// Get AI financial tips based on spending data.
async fn gemini_tips(spending_data: &str) -> Vec<String> {
    let prompt = format!(
        "Based on this spending data:\n{}\n\n\
         Give exactly 3 specific, actionable financial tips. \
         Mention actual amounts or categories where relevant. \
         Return ONLY a JSON array of 3 strings. No markdown.",
        spending_data
    );

    match gemini_api_call(prompt).await {
        Ok(raw) => {
            let cleaned = strip_code_fences(&raw);
            serde_json::from_str::<Vec<String>>(cleaned).unwrap_or_default()
        }
        Err(_) => Vec::new(),
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

/// Try the Apple Card regex first; fall back to Gemini for any other format.
async fn extract_records(file_path: &str) -> Result<Vec<Record>, String> {
    let text = pdf_extract::extract_text(file_path).map_err(|e| e.to_string())?;

    // Fast path: Apple Card regex
    let regex_records: Vec<Record> = RE
        .captures_iter(&text)
        .filter_map(|cap| {
            let amount: f64 = cap[3].replace(",", "").parse().ok()?;
            if amount <= 0.0 { return None; }
            let description = cap[2].trim().to_string();
            let category = categorize(&description);
            Some(Record { date: cap[1].to_string(), amount, description, category })
        })
        .collect();

    if !regex_records.is_empty() {
        return Ok(regex_records);
    }

    // Slow path: ask Gemini to understand any other bank format
    Ok(gemini_extract_transactions(&text).await)
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
            "No transactions found in {}. If this isn't an Apple Card statement, \
             make sure GEMINI_API_KEY is set — Gemini is used to parse other formats.",
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
    let (monthly_avg, top_expenses, spending_text) = {
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

    // ── Phase 2: Gemini tips (no lock held) ──────────────────────────────────
    let tips = if spending_text.contains('$') {
        gemini_tips(&spending_text).await
    } else {
        Vec::new()
    };

    (
        StatusCode::OK,
        Json(json!({
            "monthly_average": monthly_avg,
            "top_expenses": top_expenses,
            "tips": tips,
        })),
    )
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
        .route("/register",     post(register))
        .route("/login",        post(login))
        .route("/summary",      get(get_summary))
        .route("/transactions", get(get_transactions))
        .route("/upload",       post(upload))
        .route("/analytics",    get(get_analytics))
        .route("/cash",         post(add_cash_entry).get(get_cash_entries))
        .route("/cash/{id}",    axum::routing::delete(delete_cash_entry))
        .with_state(state)
        .layer(CorsLayer::permissive());

    let addr = "0.0.0.0:3000";
    println!("FINAD API running on {}", addr);
    println!("  POST /register | POST /login");
    println!("  GET  /summary | GET /transactions | GET /analytics  (Bearer token)");
    println!("  POST /upload | POST /cash | GET /cash | DELETE /cash/:id  (Bearer token)");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
