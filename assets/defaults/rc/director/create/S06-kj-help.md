# kj

kj is how we work with this kernel. It runs in the shell. `kj help` gives the
overview, and `kj <verb> --help` gives a verb's options.

A context is one conversation: a log of blocks, a type that selects its
instructions, a performer who takes its turns, and a reviewer who answers its
asks. A context with no performer cannot take a turn. We name a context as `.`
(this one), `.parent`, its label, or a prefix of its id.

`kj drive` starts a turn in a context. `kj wait` returns when that turn ends,
when the context raises an ask, or when the timeout passes, and it says which.
After a wait we read what came back and choose: drive again, answer the ask, or
move on.

A fork is a child context that starts with a copy of our history, and we stay
on the parent. Forks hold side work we do not want in our own history.
`kj fork --exclude` leaves chosen blocks out of the child.

Drift moves information between contexts. `kj drift push` puts text in another
context's log without running a model; a running turn there reads it after its
next tool call. `kj drift pull` has a model distill another context into ours.
`kj drift merge`, run inside a fork, summarizes the fork into its parent.

When a context needs a decision outside its loadout, it raises an ask and
waits. The ledger holds the asks, and the reviewer allows or denies each one.

Examples. Replace each `<...>` with a real value read from kj.

kj context list
See every context with its type, performer, and state.

kj rc list
See the context types this kernel has and their lifecycle scripts.

kj character list
Find the character who will perform a new context.

kj context create fix-login --type coder --as <character>
Start one piece of work in its own context. Pick the type that fits the work; coder is one type.

kj drive fix-login --prompt "<the work, what it may touch, what done looks like, and how to check it>"
Send the brief and start the turn.

kj wait fix-login --timeout 900
Wait for the turn to end or for an ask. Pass --since <cursor> to read only new blocks.

kj ledger list
See the asks waiting for an answer.

kj ledger show <ask-id>
Read an ask before we decide it.

kj ledger allow <ask-id>
Allow an ask, or refuse it with kj ledger deny <ask-id>. Run each as its own command, with nothing after it.

kj drift push fix-login "Leave src/auth alone and make the change in src/login.rs."
Correct or inform a context without running a model.

kj drift pull fix-login "List the files you changed and each check you ran, with its exit status."
Bring a distilled result from a context into ours.

kj interrupt fix-login
Stop a turn that is looping or off course. Add --immediate to stop it at once.

kj fork --name read-rc --prompt "List the rc scripts that read /config/kernel."
Do side work in a fork, then read its result with kj drift pull read-rc.

kj handoff note "Did: <...> Unfinished: <...> Next: <...>"
Leave a note for the next seat. Then kj context rotate replaces this seat with a successor that reads it.

kj context archive fix-login --confirm
Archive a context once its work is checked and reported. Its blocks stay readable with kj block list -c fix-login.
