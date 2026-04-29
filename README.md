# FINAD — Personal Finance Tracker

A full-stack personal finance application with a Rust/Axum backend and React frontend. Parses bank statements (PDF), connects to real bank accounts via Teller, auto-categorizes transactions, and provides AI-powered spending insights through the Claude API.

---

## Features

### Data Import
- **PDF statement parsing** — Apple Card, Discover, and generic bank PDFs via `pdf-extract` + regex
- **Bank sync via Teller** — connect real bank/credit card accounts and automatically import transactions
- **Manual cash entries** — log cash income and expenses not captured by statements

### Transaction Management
- Auto-categorizes transactions into 15 categories (dining, groceries, fuel, transport, shopping, entertainment, recreation, education, bills, insurance, memberships, health, automotive, travel, payment)
- **Credit card payment detection** — payments to credit card issuers are automatically identified and excluded from spending totals
- Deduplication: PDF uploads use content-based dedup; Teller transactions are keyed on their unique Teller ID and upserted on re-sync (so re-categorization works without creating duplicates)
- Incremental bank sync — cursor-based pagination means only new transactions are fetched after the first full import

### Analytics & Dashboard
- Spending by category (donut chart with legend)
- Monthly spending trend (bar chart)
- Top expenses table
- Summary stats: total spent, transaction count, category count, monthly average

### AI Chat
- Chat with Claude (claude-haiku-4-5) about your spending
- Context-aware: the AI receives your full spending summary before answering

### Auth
- JWT-based authentication (register / login)
- Per-user data isolation — all transactions, accounts, and entries are scoped to the authenticated user

---

## Tech Stack

### Backend (`finad/`)

| Crate | Purpose |
|---|---|
| `axum` | HTTP server and routing |
| `rusqlite` | SQLite database (local, file-based) |
| `reqwest` | HTTP client for Teller API and Claude API calls |
| `pdf-extract` | Extract raw text from PDF statements |
| `regex` + `once_cell` | Parse transaction lines with compiled static regexes |
| `jsonwebtoken` | JWT auth token generation and validation |
| `bcrypt` | Password hashing |
| `serde` / `serde_json` | JSON serialization |
| `tokio` | Async runtime |
| `chrono` | Date handling |
| `dotenvy` | `.env` file loading |
| `tower-http` | CORS middleware |

### Frontend (`finad-ui/`)
React + TypeScript + Vite, Recharts for charts, Axios for API calls.

---

## Project Structure

```
finad/
├── src/
│   └── main.rs          # All backend logic: API handlers, parsing, DB, auth
├── Cargo.toml
├── .env                 # API keys (not committed)
└── transactions.db      # Generated at runtime (not committed)
```

---

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) via `rustup`
- An `ANTHROPIC_API_KEY` for AI chat features
- (Optional) A [Teller](https://teller.io) application ID for live bank sync

### Environment variables

Create a `.env` file in the `finad/` directory:

```env
ANTHROPIC_API_KEY=sk-ant-...
JWT_SECRET=your_random_secret_here
```

### Run the backend

```bash
cd finad
cargo run
# Server listens on http://localhost:3000
```

### Upload a statement

Use the web UI (Upload page) or POST directly:

```bash
curl -X POST http://localhost:3000/upload \
  -H "Authorization: Bearer <token>" \
  -F "file=@statement.pdf"
```

### Sync a connected bank

```bash
# After completing Teller Connect flow in the UI:
curl -X POST http://localhost:3000/teller/sync \
  -H "Authorization: Bearer <token>"
```

---

## Database Schema

```sql
CREATE TABLE transactions (
    id          INTEGER PRIMARY KEY,
    date        TEXT NOT NULL,
    amount      REAL NOT NULL,
    description TEXT NOT NULL,
    category    TEXT NOT NULL DEFAULT '',
    user_id     INTEGER REFERENCES users(id),
    teller_id   TEXT,                          -- Teller transaction ID (bank-synced only)
    UNIQUE(date, description, amount, user_id) -- content-based dedup for PDF uploads
);

-- Separate unique index for Teller transactions keyed on their own ID
CREATE UNIQUE INDEX idx_tx_teller_id
    ON transactions(teller_id, user_id)
    WHERE teller_id IS NOT NULL;

CREATE TABLE teller_enrollments (
    id               INTEGER PRIMARY KEY,
    user_id          INTEGER NOT NULL REFERENCES users(id),
    access_token     TEXT NOT NULL,
    enrollment_id    TEXT NOT NULL,
    institution_name TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

CREATE TABLE teller_accounts (
    id                   INTEGER PRIMARY KEY,
    teller_enrollment_id INTEGER NOT NULL REFERENCES teller_enrollments(id),
    user_id              INTEGER NOT NULL REFERENCES users(id),
    account_id           TEXT NOT NULL,
    name                 TEXT NOT NULL,
    account_type         TEXT NOT NULL,
    last_four            TEXT,
    last_transaction_id  TEXT,            -- sync cursor: newest ID seen
    UNIQUE(account_id, user_id)
);

CREATE TABLE cash_entries (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id),
    date        TEXT NOT NULL,
    amount      REAL NOT NULL,
    entry_type  TEXT NOT NULL,            -- 'income' | 'expense'
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE users (
    id                INTEGER PRIMARY KEY,
    email             TEXT UNIQUE NOT NULL,
    password_hash     TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    phone             TEXT,
    security_question TEXT,
    security_answer   TEXT
);
```

---

## API Endpoints

| Method | Path | Description |
|---|---|---|
| `POST` | `/register` | Create account |
| `POST` | `/login` | Get JWT token |
| `GET` | `/transactions` | List all transactions |
| `POST` | `/upload` | Upload PDF statement |
| `GET` | `/analytics` | Spending analytics |
| `GET` | `/summary` | Text spending summary |
| `POST` | `/chat` | AI chat (Claude) |
| `GET` | `/cash` | List cash entries |
| `POST` | `/cash` | Add cash entry |
| `DELETE` | `/cash/:id` | Delete cash entry |
| `POST` | `/teller/enroll` | Connect a bank account |
| `POST` | `/teller/sync` | Sync transactions from all connected banks |
| `GET` | `/teller/accounts` | List connected bank accounts |
| `DELETE` | `/teller/accounts/:id` | Disconnect a bank account |
| `GET` | `/account` | Get account info |
| `PUT` | `/account` | Update account info |
| `PUT` | `/account/password` | Change password |

---

## Privacy

Your financial data stays local — stored in `transactions.db` on your machine. The only external calls are:
- **Teller API** — to fetch your bank transactions (requires your Teller access token)
- **Anthropic API** — your spending *summary* (not raw transactions) is sent when you use AI chat

**Never commit `.env`, `*.pdf`, `*.csv`, or `*.db`.** These are excluded in `.gitignore`.

---

## Roadmap

- [x] PDF parsing (Apple Card, Discover, generic)
- [x] SQLite storage with deduplication
- [x] JWT authentication with per-user data isolation
- [x] Transaction auto-categorization (15 categories)
- [x] Credit card payment detection
- [x] Spending analytics (by category, by month, top expenses)
- [x] Claude AI chat integration
- [x] Teller bank connection with incremental sync
- [x] Teller upsert keyed on transaction ID
- [x] Manual cash entry tracking
- [x] Web dashboard (React + Recharts)
- [ ] Support for more PDF formats (Chase, Bank of America, etc.)
- [ ] Recurring charge detection
- [ ] Budget setting and alerts
- [ ] Export to CSV

---

## License

MIT
