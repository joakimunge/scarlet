-- Development fixture: varied types and edge cases for exercising the UI.
DROP SCHEMA IF EXISTS public CASCADE;
DROP SCHEMA IF EXISTS analytics CASCADE;
CREATE SCHEMA public;
CREATE SCHEMA analytics;

CREATE TABLE customers (
  id          bigserial PRIMARY KEY,
  email       text NOT NULL UNIQUE,
  name        text,
  country     char(2),
  vip         boolean NOT NULL DEFAULT false,
  balance     numeric(12,2) NOT NULL DEFAULT 0,
  tags        text[],
  profile     jsonb,
  external_id uuid DEFAULT gen_random_uuid(),
  created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE products (
  id          serial PRIMARY KEY,
  sku         text NOT NULL UNIQUE,
  title       text NOT NULL,
  description text,
  price       numeric(10,2) NOT NULL,
  attributes  jsonb NOT NULL DEFAULT '{}'
);

CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  customer_id bigint NOT NULL REFERENCES customers(id),
  status      text NOT NULL CHECK (status IN ('pending','paid','shipped','refunded')),
  total       numeric(12,2) NOT NULL,
  note        text,
  placed_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE order_items (
  order_id    bigint REFERENCES orders(id),
  product_id  int REFERENCES products(id),
  quantity    int NOT NULL,
  unit_price  numeric(10,2) NOT NULL,
  PRIMARY KEY (order_id, product_id)
);

CREATE TABLE reviews (
  id          serial PRIMARY KEY,
  product_id  int NOT NULL REFERENCES products(id),
  customer_id bigint REFERENCES customers(id),
  rating      smallint NOT NULL CHECK (rating BETWEEN 1 AND 5),
  body        text
);

CREATE TABLE events (
  id          bigserial PRIMARY KEY,
  kind        text NOT NULL,
  payload     jsonb,
  ip          inet,
  duration    interval,
  at          timestamp NOT NULL
);

CREATE TABLE "Mixed Case Table" ("Weird Column" text, "select" int, "ünïcødé" text);

INSERT INTO customers (email, name, country, vip, balance, tags, profile)
SELECT 'user' || g || '@example.com',
       (ARRAY['Astrid Lindqvist','Björn Ek','Chen Wei','Dolores O''Brien','Émile Zola','Fatima Al-Sayed','山田太郎','Grace Hopper'])[1 + g % 8],
       (ARRAY['SE','NO','DK','DE','US','JP',NULL])[1 + g % 7],
       g % 11 = 0,
       round((random() * 10000)::numeric, 2),
       CASE WHEN g % 3 = 0 THEN ARRAY['early','beta'] WHEN g % 5 = 0 THEN '{}'::text[] END,
       CASE WHEN g % 4 = 0 THEN NULL ELSE jsonb_build_object('theme', (ARRAY['dark','light'])[1 + g % 2], 'newsletter', g % 2 = 0, 'address', jsonb_build_object('city', (ARRAY['Stockholm','Oslo','Tokyo'])[1 + g % 3], 'zip', lpad((g * 37 % 99999)::text, 5, '0'))) END
FROM generate_series(1, 2500) g;

UPDATE customers SET name = 'Someone with an absurdly long name that should never fit in a column without being truncated gracefully' WHERE id = 7;
UPDATE customers SET name = E'Line one\nLine two\ttabbed' WHERE id = 8;
UPDATE customers SET name = '🦊 Emoji Person 🚀' WHERE id = 9;
UPDATE customers SET name = '' WHERE id = 10;

INSERT INTO products (sku, title, description, price, attributes)
SELECT 'SKU-' || lpad(g::text, 5, '0'),
       (ARRAY['Wool Sweater','Riesling 2021','Guitar Strings 10-46','Priest Robe','Mechanical Keyboard'])[1 + g % 5] || ' #' || g,
       CASE WHEN g % 2 = 0 THEN repeat('A very detailed description. ', 1 + g % 6) END,
       (5 + random() * 495)::numeric(10,2),
       jsonb_build_object('color', (ARRAY['black','scarlet','ivory'])[1 + g % 3], 'sizes', jsonb_build_array('S','M','L'), 'stock', g * 7 % 120)
FROM generate_series(1, 400) g;

INSERT INTO orders (customer_id, status, total, note, placed_at)
SELECT 1 + (g * 7919) % 2500,
       (ARRAY['pending','paid','shipped','refunded'])[1 + g % 4],
       round((random() * 2000)::numeric, 2),
       CASE WHEN g % 9 = 0 THEN 'Leave at the door' END,
       now() - (g || ' minutes')::interval
FROM generate_series(1, 20000) g;

INSERT INTO order_items (order_id, product_id, quantity, unit_price)
SELECT o, 1 + (o * 13 + k) % 400, 1 + (o + k) % 4, (5 + (o % 50))::numeric(10,2)
FROM generate_series(1, 20000) o, generate_series(0, 2) k;

INSERT INTO reviews (product_id, customer_id, rating, body)
SELECT 1 + g % 400, CASE WHEN g % 10 = 0 THEN NULL ELSE 1 + (g * 31) % 2500 END, 1 + g % 5,
       (ARRAY['Lovely.', 'Would buy again', NULL, 'Arrived late but works'])[1 + g % 4]
FROM generate_series(1, 3000) g;

INSERT INTO events (kind, payload, ip, duration, at)
SELECT (ARRAY['login','logout','purchase','view','error'])[1 + g % 5],
       jsonb_build_object('session', md5(g::text), 'n', g),
       ('10.0.' || (g % 255) || '.' || (g % 253))::inet,
       ((g % 3600) || ' seconds')::interval,
       timestamp '2026-01-01' + (g || ' seconds')::interval
FROM generate_series(1, 200000) g;

INSERT INTO "Mixed Case Table" VALUES ('quoted identifiers', 1, 'åäö'), (NULL, NULL, NULL);

CREATE VIEW paid_orders AS SELECT * FROM orders WHERE status = 'paid';

CREATE TABLE analytics.daily_revenue AS
SELECT date_trunc('day', placed_at)::date AS day, count(*) AS orders, sum(total) AS revenue
FROM orders GROUP BY 1 ORDER BY 1;

CREATE MATERIALIZED VIEW analytics.top_customers AS
SELECT c.id, c.name, sum(o.total) AS spent FROM customers c JOIN orders o ON o.customer_id = c.id
GROUP BY c.id, c.name ORDER BY spent DESC LIMIT 100;

CREATE TABLE analytics.wide AS
SELECT g AS id, g AS c01, g*2 AS c02, g*3 AS c03, md5(g::text) AS c04, g*5 AS c05, g*6 AS c06, md5((g+1)::text) AS c07,
       g*8 AS c08, g*9 AS c09, g*10 AS c10, md5((g+2)::text) AS c11, g*12 AS c12, g*13 AS c13, g*14 AS c14,
       md5((g+3)::text) AS c15, g*16 AS c16, g*17 AS c17, g*18 AS c18, g*19 AS c19, md5((g+4)::text) AS c20
FROM generate_series(1, 300) g;

CREATE TABLE analytics.empty_table (id int, label text);
ANALYZE;
