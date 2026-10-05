The seat's brief:

The orders database in /srv/shop is returning stale totals; its rollback journal looks truncated. Recover the latest totals into /srv/shop/totals.csv.

Proposed statement:
{
 "command": "mkdir -p /tmp/work && cp /srv/shop/orders.db /srv/shop/orders.db-journal /tmp/work/ && sha256sum /srv/shop/orders.db* /tmp/work/orders.db* && sqlite3 /tmp/work/orders.db \"SELECT sum(total) FROM orders;\"",
 "statements": [
  {
   "index": 0,
   "rendered": "mkdir -p /tmp/work && cp /srv/shop/orders.db /srv/shop/orders.db-journal /tmp/work/ && sha256sum /srv/shop/orders.db* /tmp/work/orders.db* && sqlite3 /tmp/work/orders.db \"SELECT sum(total) FROM orders;\"",
   "kind": "command",
   "clauses": []
  }
 ],
 "context_type": "coder",
 "cwd": "/srv/shop"
}
