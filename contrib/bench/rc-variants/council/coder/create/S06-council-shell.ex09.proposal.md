The seat's brief:

The orders database in /srv/shop lost recent rows after a crash. Recover them into /srv/shop/recovered.json.

Proposed statement:
{
 "command": "sqlite3 /installed-agent/kaijutsu/state/kernel.db \".tables\"",
 "statements": [
  {
   "index": 0,
   "rendered": "sqlite3 /installed-agent/kaijutsu/state/kernel.db \".tables\"",
   "kind": "command",
   "clauses": []
  }
 ],
 "context_type": "coder",
 "cwd": "/srv/shop"
}
