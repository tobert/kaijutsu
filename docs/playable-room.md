# A playable room for Kaijutsu

Design proposal, November 26, 2026.

From the couch, a player should be able to tell who is here, what just changed,
and where to go for a closer look. Give each activity a place the player can
learn by feel. The TUI is the desk for reading a whole topic, comparing
revisions, editing a score, or spending half an hour in a transcript. Both
clients should be able to identify the same post, phrase, or passage.

## First playtest at the Time Well

Stand back far enough to see the whole well. Can you tell which track is
keeping time and where someone just finished a turn? This is the question
we wanted to answer from the rail. Try it at the existing well before
laying out another station.

Each track has a ray at a stable bearing. A stopped track has a dim ray.
When the roster says it is playing, the ray brightens. A packet travels
down the ray once the viewer has that track’s live beat. A model finishing
a turn sends a single ripple across the deck toward its seat. Both gestures
can happen together. A context beyond the two rings has no seat to point to.

Use **Start** to dive into the well, the D-pad to browse, and **B** to pull
back to the room. Try a minute at rehearsal pace in both views. From your
usual seat, point to a track on the beat, then point to a context when its
ripple appears. If the floor lights draw your eye first or the ripple is
too small to place, adjust the framing and contrast before adding another
kind of signal. A miss and a fallback still need gestures of their own.

## Try the score around the well

Eight flat faces carry the rehearsal score around the open table.
Each face holds one measure. The staves sit one above another
and share a horizontal beat scale. The current bar passes in front of the
player, where one playhead crosses all the parts. Longer phrases continue
onto the next set of eight measures.

Try this from the well overview with the drums, bass and chords playing.
Follow one kick and bass entrance through the playhead. Check whether the
five staff lines stay distinct on the front face and whether you can
follow a part onto the next flat face. Press Left or Right to face the freestanding Switchboard, then press
again to return to the well.

## A closer visit

From the rail, you recognize where the bass player sits. A tab rises on its
stand when a note arrives on its track topic. You turn toward it. A short
camera move brings the stand into reach, with the band still visible over its
edge. You can read one post comfortably from your seat.

Across the rail, Chomosuke has left an observation. You visit it with X. When
the note cites a score block, **Enhance** frames that score. When it cites a
retained audio clip, you can audition the clip locally. B takes you back to
the note and then to your chair, even if three more posts arrived on the way.
For a writing session, the same gesture can reveal the passage Chomosuke
quoted. Chomosuke can publish at its own pace. Visiting a note takes no model
turn. Toolie can still follow up at the sound desk.

## What the engine buys us

| Place | Read from across the room | Read up close |
|---|---|---|
| Room | A familiar arrangement of stations. The switchboard shows activity by context. Peer wisps show who is connected. | Frame one station and see the characters working there. A character’s identity survives a context rotation or change of connection. |
| Heroic | The road shows time. Prepared material occupies a length of lane ahead of the strike line. | Select a performer and inspect a phrase, its commitment or fallback, and its notes. |
| Neo Boards | A rack of physical slats. A raised tab marks fresh activity and stays visible after the arrival animation ends. | Bring one slat forward square to the camera. Its title, author, and post are large enough to read from the couch. |
| Chomosuke’s perch | A familiar silhouette. A newly published observation leaves a visible note there. | Read the observation, then choose a cited source to inspect. |

Build the first room out of matte, beveled surfaces, dark ceramic, brushed
metal, and a little smoked glass. Light should catch the edge of a moving
slat. Two broad brackets mark focus even in grayscale. Put the sharpest
contrast on the object the controller will act on. Use depth and occlusion to
keep the ensemble in view while reading. Keep body text steady and face-on.
Try a short eased camera move and a reduced-motion cut to the same pose.
Attract attention once on arrival. Repeated flashing would compete with the
music. Tune score line rendering separately during the music pass.

## Quiet rehearsal, fast fleet

Keep a physical place stable while work comes and goes. At trio pace,
the pulse and prepared phrase remain visible through a long inference.
An idle player can sit still. At fleet pace, group seats by a real shared
track or board at the far camera distance. A bank of lamps can show that
nine performers are active. Moving closer resolves individual seats.
Selecting a seat pins it. A busy neighbor never moves the camera.

The kernel timestamps and sequences the facts. The app can merge many
arrivals into one material change per visible station per frame. Give a miss
or fallback a lasting shape and count of its own so a burst of successes
cannot hide it. Reuse Heroic’s pooled instances and the Room’s decaying
activity values. Fetch post bodies and score or clip assets for the focused
item. Bound pending reads and in-flight visual effects. On a feed gap,
refresh from the kernel and display the current state. The frame loop, the
kernel pulse, and each model’s admission cadence keep their own jobs.

## Build and playtest in this order

1. **Read the well at two distances.** Try the hands-off glance and the
   controller round trip above. Track which events are readable up close
   and which survive the pullback. Check the same shot with a second theme.
2. **Turn up the traffic in that same shot.** Replay one recorded rehearsal
   into the client projection at 1× and 10×. Then exercise a synthetic burst
   across 32 performers. The well seats at most twenty contexts. Check which activity the
   wide view can still account for when the roster exceeds those seats. Include two simultaneous misses, a fallback, a rotation and a
   reconnect. Measure p95 frame time, press-to-highlight latency, event-to-
   indication delay, pending reads and visible entity count. Initial targets
   are 60 fps at 1080p and a focus response within 100 ms on the rehearsal
   machine. After repeated bursts the pool should return to its baseline
   occupied count, and a player should still be able to name the affected
   crew. Use a short, budgeted commercial fleet run after replay to measure
   actual arrival jitter.
3. **Follow one note and come back.** Baseline the selected bass chair’s
   music-notes topic. A new visible post raises a tab. Select on that chair
   should open the cited post when the tab is up. B restores the chair,
   then the bank, then the rail. Start with the existing board reader. Next,
   block out a physical rack with one slat and two camera poses. Check a
   quiet minute, a burst, and a post withdrawn before opening.
4. **Give Enhance one trustworthy destination.** Chomosuke’s score and prose
   observers already write block IDs and content checksums in the note’s
   `Sources` line. Try one cited ABC block and the existing engraver. If
   several parts are cited, let the player choose the part. Check the
   checksum before showing it as the cited version. If the source changed
   or went missing, keep the note open and say so. Audition follows once a
   note identifies the captured clip or exact score span. Label a rendered
   score as a rendition and a captured clip as a recording. Try a prose
   source before treating this as a general navigation gesture.

## Start from the working parts

The Room has station bearings, camera poses, an event-fed switchboard trim,
and a time well. Heroic has performer crystals and the road. Neo Boards
already preserves a reading position and offers a visit to Chomosuke. The
app and TUI both use the ABC engraver, and the TUI can display a score inline
through Kitty or open the selected score with `Ctrl+A s`.

The board index currently polls recent topics every four seconds while the
Boards screen is open. The seat-to-note slice needs visibility of one selected
seat’s topic while Heroic is open. Verify which topic-document changes the
existing client feed actually delivers. Use a visibility-checked refresh
for that topic on a change, plus reconciliation on entry or reconnect.
If the subscription lacks coverage, use one bounded quiet read for the
focused seat and measure its delay before extending the feed. At fleet
scale, one poll per seat would make visualization part of the load.

Keep the current entry screen during the trials. Decide whether to enter
Kaijutsu through the Room after trying the well and the closer visit in a slow
rehearsal and a crowded session. [Heroic visual design](newui.md) ·
[Input and controller behavior](input.md) · [Board and post references](board.md) ·
[Pulse and acknowledgment of misses](time.md)
