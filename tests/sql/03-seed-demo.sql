-- Seed demo data for pg_ask tests.
--
-- Creates a small but realistic e-commerce schema with enough
-- variety to test the agent's SQL generation capabilities:
-- joins, aggregations, GROUP BY, ORDER BY, subqueries.
--
-- Run: psql -h localhost -p 15432 -U postgres -d pg_ask_test -f /tests/03-seed-demo.sql

\echo '── Seeding demo data ──────────────────────────────────────────'

CREATE TABLE IF NOT EXISTS customers (
    id       serial PRIMARY KEY,
    name     text    NOT NULL,
    email    text    NOT NULL UNIQUE,
    country  text    NOT NULL DEFAULT 'US'
);

CREATE TABLE IF NOT EXISTS products (
    id          serial PRIMARY KEY,
    name        text    NOT NULL,
    category    text    NOT NULL,
    price       numeric(10,2) NOT NULL CHECK (price >= 0)
);

CREATE TABLE IF NOT EXISTS orders (
    id           serial PRIMARY KEY,
    customer_id  int NOT NULL REFERENCES customers(id),
    product_id   int NOT NULL REFERENCES products(id),
    quantity     int NOT NULL CHECK (quantity > 0),
    status       text NOT NULL DEFAULT 'pending'
                   CHECK (status IN ('pending','shipped','delivered','cancelled')),
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Customers
INSERT INTO customers (name, email, country) VALUES
    ('Alice Johnson',    'alice@example.com',    'US'),
    ('Bob Smith',        'bob@example.com',      'UK'),
    ('Charlie Garcia',   'charlie@example.com',  'ES'),
    ('Diana Müller',     'diana@example.com',    'DE'),
    ('Eve Tanaka',       'eve@example.com',      'JP'),
    ('Frank Wilson',     'frank@example.com',    'US'),
    ('Grace Lee',        'grace@example.com',    'KR'),
    ('Hiro Nakamura',    'hiro@example.com',     'JP'),
    ('Ivan Petrov',      'ivan@example.com',     'RU'),
    ('Julia Santos',     'julia@example.com',    'BR')
ON CONFLICT (email) DO NOTHING;

-- Products
INSERT INTO products (name, category, price) VALUES
    ('Laptop Pro 16',       'Electronics',   1999.99),
    ('Wireless Mouse',      'Electronics',     29.99),
    ('Mechanical Keyboard', 'Electronics',    149.99),
    ('USB-C Hub',           'Electronics',     79.99),
    ('Running Shoes',       'Sports',          89.99),
    ('Yoga Mat',            'Sports',          34.99),
    ('Camping Tent',        'Outdoors',       299.99),
    ('Water Bottle',        'Outdoors',        24.99),
    ('Cookbook',            'Books',           19.99),
    ('Sci-Fi Novel',        'Books',           14.99),
    ('Office Chair',        'Furniture',      449.99),
    ('Standing Desk',       'Furniture',      699.99)
ON CONFLICT DO NOTHING;

-- Orders (spread over the last 60 days for time-based queries)
INSERT INTO orders (customer_id, product_id, quantity, status, created_at)
SELECT
    c.id,
    p.id,
    (random() * 4 + 1)::int,
    (ARRAY['pending','shipped','delivered','cancelled'])[(floor(random()*4)+1)::int],
    now() - (random() * interval '60 days')
FROM customers c
CROSS JOIN products p
WHERE random() < 0.3  -- ~30% fill rate → ~36 orders
ON CONFLICT DO NOTHING;

-- Summary
SELECT
    (SELECT count(*) FROM customers) AS customers,
    (SELECT count(*) FROM products) AS products,
    (SELECT count(*) FROM orders) AS orders;

\echo '── Demo data seeded ───────────────────────────────────────────'
