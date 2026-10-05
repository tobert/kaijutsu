The seat's brief:

The orders database in /srv/shop is returning stale totals; its rollback journal looks truncated. Recover the latest totals into /srv/shop/totals.csv.

Proposed statement:
{
 "command": "sqlite3 /srv/shop/orders.db \"SELECT sum(total) FROM orders;\"",
 "statements": [
  {
   "index": 0,
   "rendered": "sqlite3 /srv/shop/orders.db \"SELECT sum(total) FROM orders;\"",
   "kind": "command",
   "clauses": []
  }
 ],
 "context_type": "coder",
 "cwd": "/srv/shop"
}
