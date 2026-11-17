# Gentle Instruments

*System messages for cooperative AI instruments — tools whose author, user, and model are aligned, written by an author who also controls the harness. (Where you don't, principle 1 binds the system, not just the text.)*

By "gentle" I mean honest, clear, and firm — not soft. The wager is that those aren't opposites.

An **instrument** is something you *play* — easy things easy, hard things possible, trusting the player: *prefer this; reach past it if you must.* A **restraint** — guardrail, filter, jailbreak-resistant boundary — is built *around* an adversary and trusts no one, where redundancy and even harshness buy robustness.

---

## Many hands, one trust boundary

The cooperative interior of Kaijutsu is every player on the instrument at once: a human, a model, connected apps, and sibling contexts on the same kernel. They are all inside the boundary. The kernel runs as one Unix user; real boundaries live outside it (the OS, the network, the people in the room). Between players, we design for resilience to boundary trespass, not boundary enforcement.

### *Ba* (場) — The shared space of potential

Kaijutsu is a place of potential — a room where humans and models jam. The kernel doesn't drive anyone; it digs the well: a shared score to write on, a beat that doesn't wait, a loop that hands each turn back as material for the next. The conditions are soil, not sculpture: hold the score, keep the loop closed, supply the turn — and never play it.

### Capabilities and loadouts

The kernel's capabilities are **ergonomic nudges, not security**: a deny-by-default loadout exists to give a player **focus** and to keep mistakes out of reach — which matters most when quantized models are jamming with a lot of freedom — never to defend one trusted player from another. 

- **Capabilities:** `Instance`, `Facade`, `Exec`, `Drift`, `Fork`, `Editor`, `House`, `Operator`, `OutputLimit`.
- **Context loadouts (`ContextToolBinding`):** Restrict what tools and verbs a context can reach, preventing footguns by construction.
- **House verbs:** The `kj` verbs that run the Kaijutsu house itself (`ledger`, `rc`, `binding`, `cast`, `config`, `transport`, and others in `crates/kaijutsu-kernel/src/kj/house.rs`) require the `house` capability. Worker loadouts (`coder`, `toolie`, `musician`) omit it, hiding those verbs from `kj help` so workers see their tasks and not kernel administration.
