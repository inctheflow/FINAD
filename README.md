# FINAD — Financial Advice CLI

A Rust command-line tool that parses Apple Card PDF and CSV bank statements, extracts transactions, and stores them in a local SQLite database. Built as a learning project in Rust, with the final goal of AI-powered personal financial analysis using the Claude API.

---

## Final Goal

FINAD will eventually:
- Parse statements from multiple banks and card types
- Auto-categorize transactions (dining, transport, shopping, subscriptions, etc.)
- Detect spending patterns and recurring charges
- Generate month-over-month comparisons
- Send spending summaries to the **Claude API** for personalized, actionable financial advice
- Present everything through a web dashboard with charts and insights

---

## Current Features

- Parses Apple Card PDF statements using `pdf-extract` and regex
- Reads CSV bank exports via `serde` deserialization
- Stores transactions in a local SQLite database with duplicate prevention
- Accepts multiple PDF files in a single run
- Gracefully skips failed files and continues processing the rest

---

## Tech Stack

| Crate | Purpose |
|---|---|
| `pdf-extract` | Extract raw text from PDF statements |
| `csv` + `serde` | Deserialize CSV exports into Rust structs |
| `regex` + `once_cell` | Parse transaction lines with a compiled static regex |
| `rusqlite` | Store and query transactions in a local SQLite database |
| `std::env` | Accept file paths as command-line arguments |

---

## Project Structure

```
finad/
├── src/
│   └── main.rs        # All parsing, storage, and CLI logic
├── Cargo.toml         # Dependencies
├── Cargo.lock
├── .gitignore
└── transactions.db    # Generated at runtime — not committed
```

---

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) installed via `rustup`
- An Apple Card PDF statement (downloaded from Wallet app or card.apple.com)

### Install and run

```bash
# Clone the repo
git clone https://github.com/yourusername/finad.git
cd finad

# Build
cargo build

# Run with one or more PDF statements
cargo run -- may_statement.pdf june_statement.pdf

# Run with all PDFs in the current folder
cargo run -- *.pdf
```

### Expected output

```
  may_statement.pdf  → 10 transactions saved
  june_statement.pdf → 12 transactions saved

Total: 22 transactions saved to database
```

Transactions are stored in `transactions.db` in the project root. Open it with [DB Browser for SQLite](https://sqlitebrowser.org/) to inspect your data.

---

## Database Schema

```sql
CREATE TABLE transactions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    date        TEXT NOT NULL,
    description TEXT NOT NULL,
    amount      REAL NOT NULL,
    category    TEXT NOT NULL DEFAULT '',
    UNIQUE(date, description, amount)
);
```

The `UNIQUE` constraint prevents duplicate rows when running the same statement twice.

---

## Privacy

Your financial data never leaves your machine. No data is sent anywhere in the current version. When the Claude API integration is added, only aggregated spending summaries (not raw transactions) will be transmitted.

**Never commit your statements or database.** The `.gitignore` excludes `*.pdf`, `*.csv`, and `*.db` by default.

---

## Roadmap

- [x] PDF parsing with regex
- [x] CSV import
- [x] SQLite storage with deduplication
- [x] Multi-file CLI input
- [ ] Transaction categorization
- [ ] Spending analysis queries (by month, by category, top merchants)
- [ ] Claude API integration for AI financial advice
- [ ] Web dashboard with charts (React + Recharts)
- [ ] Support for other banks (Chase, Bank of America, etc.)

---

## What I Learned Building This

This project was built as a hands-on way to learn Rust. Concepts covered so far:

- Structs, enums, and `#[derive]` macros
- Error handling with `Result`, `?`, and `Box<dyn Error>`
- Ownership, borrowing, and slices (`&[Record]`)
- Static values with `once_cell::sync::Lazy`
- Regex compilation and named capture groups
- Serde deserialization from CSV
- SQLite with `rusqlite`, parameterized queries, and `UNIQUE` constraints
- CLI argument parsing with `std::env::args()`
- Graceful per-file error handling with `match`

---

## License

MIT