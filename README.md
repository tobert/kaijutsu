# 会術 Kaijutsu

*"The Art of Meeting"*

Kaijutsu is an agentic interface and kernel that offers a crdt-all-the-things
approach to collaborative editing with multiple models and users participating
in real time. The 会術 ui is built on Bevy 0.18 with Glyphon for text rendering.
The kernel relies on [a fork of diamond-types][dt-fork] that completes and
extends map and register types. We will upstream that when we have a moment.

[dt-fork]: https://github.com/tobert/diamond-types/tree/feat/maps-and-uuids

## Status

This is a friends & family release. MIT license so if you wanna fork and try
it, cool, but I (Amy Tobey) haven't put much effort into making it work on any
other machine yet.

If CRDTs excite you and cargo build isn't scary, this might be for you. If you
don't know what that is, please come back later and we'll explain why it's cool
and show you a demo.

-Amy
