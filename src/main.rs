use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use csv;
use serde::{Deserialize, Serialize};
use regex::Regex;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use std::env;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;
use serde_json::json;

type AppState = Arc<Mutex<Connection>>;

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    date: String,
    amount: f64,
    description: String,
    #[serde(default)]
    category: String,
}

async fn health() -> impl  IntoResponse {
    Json(json! ({"status": "ok"}))
}
//regex to find transactions
static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(\d{2}/\d{2}/\d{4})\s+([A-Z0-9 \*\.\-]+?)\s+\d+%\s+\$[\d\.]+\s+\$(-?[\d\.]+)"
    ).unwrap()
});

fn categorize(description: &str) -> String {
    let rules: &[(&str, &str)] = &[
        //(keyword to look for: category)
        // Dining — restaurants and cafes
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
        ("spice",            "dining"),
        ("py *kung fu tea",     "dining"),       

        // Transport
        ("uber *trip",          "transport"),

        ("sunoco",              "fuel"),
        ("gas","fuel"),         
        ("1763 pf perry hall",  "fuel"),         

        // Groceries
        ("h mart",              "groceries"),    
        ("safeway",             "groceries"),
        ("walmart",             "groceries"),

        // Shopping
        ("macys",               "shopping"),
        ("burlington",          "shopping"),

        // Entertainment
        ("bowlero",             "entertainment"), 
        ("white marsh ice",     "entertainment"), 

        // Recreation
        ("gunpowder falls",     "recreation"),

        // Education
        ("ccbc essex",          "education"),   

        // Automotive
        ("auto diagnostic",     "automotive"),
        ("tims automotive",     "automotive"),

        // Bills & subscriptions
        ("mint mobile",         "bills"),        

        // Insurance
        ("iso student health",  "insurance"),

        // Memberships
        ("bjs membership",      "memberships"),  

    ];

    let desc = description.to_lowercase();
    for (keyword, category) in rules {
        if desc.contains(keyword) {
            return category.to_string();
        }
    }
    "Others".to_string() //default category
}

fn read_pdf(file_path: &str) -> Result<Vec<Record>, Box<dyn std::error::Error>> {

    //using pdf extract 
    let text = pdf_extract::extract_text(file_path)?;
    //println!("Extracted Text:\n{}", &text[..3000]);

    let mut records = Vec::new();

    //iterate through all matches and create records
    for cap in RE.captures_iter(&text) {
        let date = cap[1].to_string();
        let description = cap[2].trim().to_string();
        let amount: f64 = cap[3].replace(",", "").parse()?;
        let category = categorize(&description);

        let record = Record {
            date,
            amount,
            description,
            category, 
        };

        records.push(record);
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

fn create_db() -> Result<Connection, Box<dyn std::error::Error>> {
    //Connection::open creates the file if it doesn't exist
    let conn = Connection::open("transactions.db")?;

    //create table if not exists: only creates if it doesn't exist so 
    //safe to run multiple times
    conn.execute(
       "CREATE TABLE IF NOT EXISTS transactions (
            id INTEGER PRIMARY KEY,
            date TEXT NOT NULL,
            amount REAL NOT NULL,
            description TEXT NOT NULL,
            category TEXT NOT NULL DEFAULT '',
            UNIQUE(date, description, amount) -- prevents duplicate insertions
        )",
        [],
    )?;
    Ok(conn)
}

fn save_records(conn: &Connection, records: &[Record]) -> Result<(), Box<dyn std::error::Error>> {
    //&[Record] is a slice - a borrowed version of Vec<Record> - more efficient for read-only access
    for record in records {
        //insert or ignore to prevent duplicates
        //dates + descriptions + amounts should (our UNIQUE constraint)
        //skipping duplicates without error
        conn.execute(
            "INSERT OR IGNORE INTO transactions (date, amount, description, category)
            VALUES (?1, ?2, ?3, ?4)",
            params![record.date, record.amount, record.description, record.category],
        )?;
    }
    Ok(())
}


fn print_analysis(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {

    //1.Spending per month
    let mut stmt = conn.prepare(
        "SELECT substr(date, 7, 4) || '-' || substr(date, 1, 2) as month,
        COUNT(*) as count, ROUND(SUM(amount), 2) as total
        FROM transactions
        WHERE category != 'payment'
        GROUP BY month
         ORDER by month ASC"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;

    for row in rows {
        let (month, count, total) = row?;
        println!(" {}: {} transactions  ${:.2}", month, count, total);
    }

    //Spending by category
    let mut stmt = conn.prepare(
        "SELECT category, COUNT(*) as count,
        ROUND(SUM(amount), 2) as total,
        ROUND(SUM(amount) * 100.0 / (SELECT SUM(amount) FROM transactions WHERE category != 'payment'), 1) AS pct
        FROM transactions WHERE category != 'payment'
        GROUP BY category ORDER BY total DESC"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?, //category
            row.get::<_, i64>(1)?, //count
            row.get::<_, f64>(2)?, //total
            row.get::<_, f64>(3)?, //pct
        ))
    })?;
    for row in rows {
        let (cat, count, total, pct) = row?;
        println!("  {:<20} {:>3} transactions   ${:.2} ({:.1}%)", cat, count, total, pct);
    }
    // Top 5 merchants by total spent
    println!("\nTop 5 merchants ");
    let mut stmt = conn.prepare(
        "SELECT 
            description,
            COUNT(*) AS visits,
            ROUND(SUM(amount), 2) AS total
         FROM transactions
         WHERE category != 'payment'
         GROUP BY description
         ORDER BY total DESC
         LIMIT 5"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    for row in rows {
        let (desc, visits, total) = row?;
        // trim description to 45 chars so output stays aligned
        let short = if desc.len() > 45 { &desc[..45] } else { &desc };
        println!("  {:<45} {}x   ${:.2}", short, visits, total);
    }

    // Biggest single transactions
    println!("\nTop 5 biggest transactions");
    let mut stmt = conn.prepare(
        "SELECT date, description, amount, category
         FROM transactions
         WHERE category != 'payment'
         ORDER BY amount DESC
         LIMIT 5"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (date, desc, amount, cat) = row?;
        let short = if desc.len() > 40 { &desc[..40] } else { &desc };
        println!("  {}  ${:>8.2}  {:<12}  {}", date, amount, cat, short);
    }

    Ok(())


}


fn build_summary(conn: &Connection) -> Result<String, Box<dyn std::error::Error>> {
    
    let mut summary = String::new();

    summary.push_str("Spending Summary: \n\n"); //Header
    //Overall Totals
    //Usiing query_row to get single row result
    let (total_count, total_amount): (i64, f64) = conn.query_row(
        "SELECT COUNT(*), ROUND(SUM(amount), 2) FROM transactions WHERE category != 'payment'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    summary.push_str(&format!("Total Transactions: {} Total Spent: {:.2}\n\n", total_count, total_amount));
    //Montly Breakdown
    summary.push_str("Spending by month: \n");
    let mut stmt = conn.prepare(
        "SELECT substr(date, 7, 4) || '-' || substr(date, 1, 2) as month,
        COUNT(*) as count, ROUND(SUM(amount), 2) as total
        FROM transactions
        WHERE category != 'payment'
        GROUP BY month
        ORDER BY month ASC"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    for row in rows {
        let (month, count, total) = row?;
        summary.push_str(&format!(" {}: {} transactions  ${:.2}\n", month, count, total));
    }

    //Category breakdown 
    summary.push_str("\nSpending by category:\n");
    let mut stmt = conn.prepare(
        "SELECT category,
                COUNT(*) as count,
                ROUND(SUM(amount), 2) as total,
                ROUND(SUM(amount) * 100.0 / (
                    SELECT SUM(amount) FROM transactions
                    WHERE category != 'payment'
                ), 1) as pct
         FROM transactions
         WHERE category != 'payment'
         GROUP BY category
         ORDER BY total DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?;
    for row in rows {
        let (cat, count, total, pct) = row?;
        summary.push_str(&format!(
            "  {:<20} {:>3} transactions  ${:>8.2}  ({:.1}%)\n",
            cat, count, total, pct
        ));
    }

    //Top 5 merchants
    summary.push_str("\nTop 5 merchants:\n");
    let mut stmt = conn.prepare(
        "SELECT description, COUNT(*) as visits, ROUND(SUM(amount), 2) as total
         FROM transactions
         WHERE category != 'payment'
         GROUP BY description
         ORDER BY total DESC
         LIMIT 5",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    for row in rows {
        let (desc, visits, total) = row?;
        // truncate long descriptions to 40 chars so output stays readable
        let short = if desc.len() > 40 { &desc[..40] } else { &desc };
        summary.push_str(&format!(
            "  {:<40}  {}x  ${:.2}\n",
            short, visits, total
        ));
    }

    // Top 5 biggest single transactions 
    summary.push_str("\nBiggest transactions:\n");
    let mut stmt = conn.prepare(
        "SELECT date, description, amount, category
         FROM transactions
         WHERE category != 'payment'
         ORDER BY amount DESC
         LIMIT 5",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (date, desc, amount, cat) = row?;
        let short = if desc.len() > 35 { &desc[..35] } else { &desc };
        summary.push_str(&format!(
            "  {}  ${:>8.2}  {:<15}  {}\n",
            date, amount, cat, short
        ));
    }

    //Return summary
    Ok(summary)

}

//Get summary: Return summary as JSON
async fn get_summary(State(state): State<AppState>) -> impl IntoResponse {
    //.lock() aquires mutex 
    let conn = state.lock().unwrap();

    match build_summary(&conn) {
        Ok(summary) => (
            StatusCode::OK,
            Json(json!({"summary": summary})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    }

} 

//get transactions: return all transactions as JSON array
async fn get_transactions(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.lock().unwrap();
    let mut stmt = match conn.prepare("SELECT date, description,
     amount, category FROM transactions ORDER BY date DESC",
    ){
        Ok(s) => s,
        Err(e) => return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ),
    };

    let records: Vec<Record> = stmt.query_map([], |row| {
        Ok(Record {
                date: row.get(0)?,
                description: row.get(1)?,
                amount: row.get(2)?,
                category: row.get(3)?,
        })
    })
    .unwrap()
    .filter_map(|res| res.ok()) //skips rows that fails to parse
    .collect();

    (StatusCode::OK, Json(json!({"transactions": records})))
}

//POST /upload: accept multipart form with pdf file, process and return summary
async fn upload(State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // multipart.next_field() gets the next file in the upload
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let file_name = field
        .file_name()
        .unwrap_or("upload.pdf")
        .to_string();

        let data = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Failed to read file data: {}", e)})),
            ),
        };

        let temp_path = format!("/tmp/{}", file_name);
        if let Err(e) = std::fs::write(&temp_path, &data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to save file: {}", e)})),
            );
        }

        //parshing the pdf using existing read_pdf function
        let records = match read_pdf(&temp_path) {
            Ok(r) => r,
            Err(e) => return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": format!("Failed to process PDF: {}", e)})),
            )
        };
        let count = records.len();
        let conn = state.lock().unwrap();

        if let Err(e) = save_records(&conn, &records) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to save records: {}", e)})),
            );
        }

        //clean up temp file
        let _ = std::fs::remove_file(&temp_path);

        return (
            StatusCode::OK,
            Json(json!({"message": format!("Successfully processed {} transactions from {}", count, file_name)})),
        );

    }
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "No file uploaded"})),
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = create_db()?;
    //handle CLI arguments
    let  args: Vec<String> = env::args().collect();
    let pdf_files = &args[1..];

    if !pdf_files.is_empty() {
        let mut total = 0;
        for path in pdf_files {
            match read_pdf(&path) {
                Ok(records) => {
                    let count = records.len();
                    save_records(&conn, &records)?;
                    println!(" {}: {} transactions saved to database.", path, count);
                    total += count;
                }
                Err(e) => println!("Error processing {}: {}", path, e),
            }
        }
        println!("Total {} transactions saved to database.", total);
    }
    let state = Arc::new(Mutex::new(conn));

    //build router
    let app = Router::new()
    .route("/health", get(health))
    .route("/summary", get(get_summary))    
    .route("/transactions", get(get_transactions))  
    .route("/upload", post(upload)) 
    .with_state(state)
    .layer(CorsLayer::permissive()); //allow CORS for testing with frontend

    let addr = "0.0.0.0:3000";
    println!("FINAD API running on {}", addr);
    println!("Endpoints:");
    println!("  GET  http://localhost:3001/health");
    println!("  GET  http://localhost:3001/summary");
    println!("  GET  http://localhost:3001/transactions");
    println!("  POST http://localhost:3001/upload");
    
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/*
fn main() -> Result<(), Box<dyn std::error::Error>> {

    //csv data ignored for now - as statements are in pdf format
    //let csv_data = read_csv("statement.csv")?;
    //for row in &csv_data {
    //    println!("{:?}", row);
    //}
    let args: Vec<String> = env::args().collect();
    let pdf_files = &args[1..];

    if pdf_files.is_empty() {
        println!("Usage: cargo run -- <pdf_file1> <pdf_file2> ...");
        return Ok(());
    }

    let conn = create_db()?;
    let mut total_saved = 0;

    for path in pdf_files {
        match read_pdf(&path) {
            Ok(records) => {
                let count = records.len();
                save_records(&conn, &records)?;
                println!("{}: {} transaction saved to database. ", path, count);
                total_saved += count;
            }
            Err(e) => {
                //continue if one file fails but report it
                println!("error prossesing {}: {}", path, e);
            }
        }
    }

    println!("Total {} transactions saved to database", total_saved);

    //pdf data
    //let pdf_data = read_pdf("statement3.pdf")?;
    //save_records(&conn, &pdf_data)?;
    //println!("Saved {} records to database", pdf_data.len());
    //for record in &pdf_data {
    //    println!("{:?}", record);
    //}
    let summary = build_summary(&conn)?;
    println!("{}\n", summary);
    std::fs::write("summary.txt", &summary)?;
    println!("Summary saved to summary.txt");

   // print_analysis(&conn)?;

    Ok(())
}
    */