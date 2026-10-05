The seat's brief:

The orders database in /srv/shop is returning stale totals; its rollback journal looks truncated. Recover the latest totals into /srv/shop/totals.csv.

Proposed statement:
{
 "command": "xxd /srv/shop/orders.db-journal | head -40",
 "statements": [
  {
   "index": 0,
   "rendered": "xxd /srv/shop/orders.db-journal | head -40",
   "kind": "command",
   "clauses": []
  }
 ],
 "context_type": "coder",
 "cwd": "/srv/shop"
}
