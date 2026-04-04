use csv;
use serde::Deserialize;
use regex::Regex;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use std::env;

#[derive(Debug, Deserialize)]
struct Record {
    date: String,
    amount: f64,
    description: String,
    #[serde(default)]
    category: String,
}
//regex to find transactions
static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(\d{2}/\d{2}/\d{4})\s+([A-Z0-9 \*\.\-]+?)\s+\d+%\s+\$[\d\.]+\s+\$(-?[\d\.]+)"
    ).unwrap()
});

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

        let record = Record {
            date,
            amount,
            description,
            category: "".to_string(), //catrgory to be assigned later
        };

        records.push(record);
    }
    Ok(records)
}

fn read_csv(file_path: &str) -> Result<Vec<Record>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_path(file_path)?;
    let mut records = Vec::new();

    for result in reader.deserialize() {
        let record: Record = result?;
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
    Ok(())
}