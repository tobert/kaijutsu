# kj

kj is how we work with this kernel. It runs in the shell. `kj help` gives the
overview, and `kj <verb> --help` gives a verb's options.

A context is one conversation: a log of blocks, performed by one character.
The performer takes its turns, and the rc scripts in the performer's directory
give the context its instructions. A context answers to a senpai, another
context. The senpai's character answers its asks. A context with no performer
cannot take a turn. We name a context as `.` (this one), `.parent`, its label, or a
prefix of its id.

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
A drift between a context and its senpai, either way, needs no ask. A fork answers to the
context it forked from, so the same holds between a context and its fork.

When a context needs a decision outside its loadout, it raises an ask and
waits. The ledger holds the asks, and the senpai's character allows or denies each one.

Examples. Replace each `<...>` with a real value read from kj.

kj context list
See every context with its performer and state.

kj rc list --dir coder
See the lifecycle scripts one rc directory holds. A character's rc directory is in kj character show <name>.

kj character list
Find the character who will perform a new piece of work. The coder character changes code; if the character we need is missing, we ask the human.

kj context create coder --name fix-login
Start one piece of work as a new context for a character. Without --name the label is the character's name. The context runs the character's rc directory and model. It answers to us, so its asks come to us. Pick the character that fits the work; coder is one character.

kj context list --senpai .
See the live contexts that answer to us.

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

kj drift push .senpai "Done: <...> Checked: <...> Open: <...>"
Report to our senpai. `.senpai` names it, so there is no context id to type.

kj interrupt fix-login
Stop a turn that is looping or off course. Add --immediate to stop it at once.

kj fork --name read-rc --prompt "List the rc scripts that read /config/kernel."
Do side work in a fork, then read its result with kj drift pull read-rc.

kj handoff note "Did: <...> Unfinished: <...> Next: <...>"
Leave a note for the next banto. Then kj context rotate replaces this context with a successor that reads it.

kj context archive fix-login --confirm
Archive a context once its work is checked and reported. Its blocks stay readable with kj block list -c fix-login.
