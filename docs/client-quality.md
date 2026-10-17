# Device-local client quality

stui records client experience in `$XDG_CACHE_HOME/st3/stui/slo` (otherwise
`~/.cache/st3/stui/slo`). `st ui --slo` prints the retained hours and exits before
configuration, connections or terminal setup. It creates no directory. These
measurements never leave the device. The summary contains no person, conversation
text, resource identifiers or paths. Its only identifying fields are host and build.

Each JSON line has exactly these fields:

| Field | Value |
| --- | --- |
| `version` | `1` |
| `window_start` | Unix seconds at the start of the local clock hour |
| `host` | Device hostname |
| `build` | Client build version |
| `bucket_upper_ms` | `[1,2,4,8,16,32,64,128,256,512,1024,2048,4096,8192,16384,32768,null]` |
| `counters` | Fixed names mapped to integer counts; durations use milliseconds |
| `histograms` | Fixed names mapped to seventeen integer bucket counts |
| `targets` | Initial target names mapped to numeric limits |

Buckets have inclusive upper bounds. Fractional milliseconds round upward. The
last bucket holds everything over 32768 ms. Percentiles printed by `--slo` are
bucket upper bounds, not interpolated times. The starting targets are guesses for
a week of measurement, listed together in `crates/stui/src/ui/slo.rs`.

The counters are `active_ms`, `panics`, `silent_exits`, `launches`,
`first_screen_top_bar_flips`, `first_screen_placeholder_flips`,
`conversation_opens`, `open_not_bound`, `open_remote_unavailable`,
`open_cannot_load`, `drafts_restored_unclean`, `not_live_ms`,
`direct_attach_attempts`, `direct_attach_success`, `gateway_attach`, `terminal_open_errors`,
`dead_terminal_tabs`, and `measurement_overflow`.

The histograms are `first_frame_ms`, `first_real_state_ms`,
`conversation_open_ms`, `key_to_redraw_ms`, `freshness_ms`, `reconnect_ms`,
`terminal_open_ms`, and `alert_landed_ms`.

Launch timing starts at entry to main. Real state requires `World.settled` and
successful terminal presentation. First-screen flips count changes in the live
status and attention/agent loading masks until that state is presented.
Conversation opens start with the ordinary subscription intent and finish when
entries are visible after successful presentation; cancelled opens have no success
sample. Open failures retain the feed's typed refusal or transcript-not-bound notice.
Focused input keys have individual timestamps in a fixed array. Freshness starts
when the feed receives a decoded window, before maintaining its view, and ends
after a frame displays that window's rows (or the classic alert badge). Hidden
windows produce no latency sample. Reconnects end when live state is presented.
Direct attempts count only devices with a local daemon; ordinary paired-device
gateway routes do not reduce the direct-route success percentage. Terminal opens
end when an attached native screen is visibly presented. Dead tabs
count stream loss or exit, once per transition. Alert landing requires successful action confirmation and a subsequent
non-truncated attention window to show the answered card gone or resolved, followed
by successful presentation; failed actions do not produce a success sample.

A dedicated thread wakes at the next clock-hour boundary and on shutdown. It adds
active time and drains atomics into daily JSONL files. Shutdown adds a partial hour;
a later process adds to that same hour under a disk lock. At most 336 hour rows
remain across at most fifteen daily files. Daily reads are capped at 128 KiB and
24 lines. A failed cache-directory creation disables the writer for that run.
Panic counting writes synchronously from the existing panic hook, using the same
fixed summary; the panic text stays in the existing separate panic log. Run markers
distinguish a known panic from an otherwise silent exit.

An unclean kill can lose the counters and active time since the last completed
hour, and recovery counts its stale run marker in the next launch's hour. Restored
drafts are counted only after such an unclean run. Multiple simultaneously running
clients contribute active time separately, as active client hours. A build field
names the most recent contributor to that hour. Fixed timestamp-array exhaustion
increments `measurement_overflow`; it never allocates extra hot-path storage.

Allocation verification uses stui's thread-local counting global allocator:
compare one hundred identical warmed terminal draws with and without quality
recording. The renderer's existing allocations must be identical, so the new
input and presentation accounting adds zero. Histogram boundary, fixed line
shape, unavailable directory, same-hour merging and fourteen-day rotation tests
run alongside it.

## Phone

The iOS app writes the same version-1 line with shared bucket and target definitions
in `clients/typescript/st3-views/slo.ts`. A test compares its complete empty summary
against stui's golden JSON fixture. `host` is the generic device class `ios`; `build`
is the app version and build. Member names and gateway addresses never enter a
quality line. Quality has its own recorder and storage ports, separate from the
existing observations reporter; it has no network, client or graph port.

AsyncStorage key `st3.quality.hours.v1` contains hourly JSON lines, with at most 336
rows and a 2 MiB input limit. Startup parses each retained line once, checks its fixed
shape, and merges partial hours. Writes run on hour boundaries and when the app
leaves the foreground. A rejected storage operation is ignored. Timers are cancelled
in the background; the next foreground schedules its next local hour boundary.
Fleet's Quality section subscribes while focused to new hourly history and shows
only the previous clock hour and the last 24 completed clock hours. An hour without
samples displays a dash. Green meets the stored target, red misses it. Daily totals
use the last contributing hour's targets. There is no Quality poll or remote read.

The phone uses single-thread JavaScript callbacks and fixed typed numeric arrays
for counters, histogram buckets, 64 pending keys, 128 received windows and 256
landed answers. Hot input and presentation methods create no arrays, strings, objects or
closures. JSON serialization, history merging and draft persistence happen outside
these methods. The React integration uses a reusable animation callback, two
animation frames after a commit, as a native paint opportunity. This is a presentation
estimate, not a native compositor acknowledgment; no simulator/device measurement
was performed by this change.

Startup uses React Native's `performance.rnStartupTiming.startTime`, in the same
monotonic clock as input and receipt measurements. Missing native startup metadata
omits startup latency samples and increments `measurement_overflow`. It does not
substitute a later startup clock. Real state requires both fresh attention and agent
windows from st and an online presentation. Conversation success requires a native
list viewability callback for an actual entry, call or bundle; a loading/empty row
is not success. Conversation and terminal intent clocks begin on focus, including a wait for the
feed to become available. Terminals require a visible, nonzero viewport and confirmed attach.
Receipt time is captured before feed view maintenance; only windows represented
by rows, an agent strip or the visible native alert badge yield freshness samples.
A frame received after a React commit waits for a commit containing that frame.

Direct terminal counters apply only when the selected Fabric path is known to be
direct at attach time. An unknown path is not claimed as direct. Other confirmed
attaches, including the saved gateway used after Fabric fails, count as
`gateway_attach`. Normal background suspension and intentional terminal closes do
not count as dead tabs; unexpected stream loss counts once per attached transition.
The existing content-free `onTiming` diagnostic remains separate from visible
conversation/terminal SLO samples. Alert landing requires successful action
confirmation and a complete attention window showing the card gone or resolved,
then an alert or badge presentation; truncated windows and failed actions are not
successes.

`st3.quality.run.v1` holds only a foreground/clean/panic marker. Foreground time,
non-live time and pending presentation samples stop in the background. A stale
foreground marker counts a silent exit on the next run. Fatal JS errors count
panics and preserve a panic marker; an uncatchable native crash or OS termination
is inferred as a silent exit. The unfinished hour and a fatal error whose storage
write could not finish can be lost, as with other best-effort device diagnostics.

Unsent text uses the separate `st3.drafts.local.v1` key, at most 24 drafts and 2 MiB.
It includes the actor and draft targets for recovery; neither that content nor its
identifiers enter a Quality summary. Only matching actors recover it; a changed
actor cancels an in-flight restore. Newly typed drafts are preserved, and leaving
the actor clears stored drafts. Serialization runs after composer changes and on
background events, outside the key handler. One write is in flight; further edits
maintain one latest pending view instead of queuing full draft snapshots. `drafts_restored_unclean` counts the
saved drafts actually hydrated after a stale foreground or panic marker.

## Fractal web workflow latency

Fractal web publishes content-free platform User Timing measures, without a kit
dependency. Observe them with `new PerformanceObserver(callback).observe({
type: 'measure', buffered: true })` or the browser Performance panel:

| Measure | Start → completion |
| --- | --- |
| `fractal:sidebar_interactive` | Document time zero → first observed, interactive sidebar roster |
| `fractal:switch_to_tail` | Sidebar agent selection → selected transcript content at the tail |
| `fractal:open_transcript` | Direct `/w/agent/…` document navigation → first transcript content |
| `fractal:keystroke_paint` | Composer textarea input event → next paint opportunity |
| `fractal:resources_panel` | Resources open → both queue and resource content observed |

Completion uses an animation frame followed by a task, not a compositor
presentation timestamp. Transcript empty states count as real content; waiting
and unavailable placeholders do not. Switching cancels an unfinished selection.
Leaving the transcript for another workspace view also cancels pending direct-open
and switch samples; returning to Thread does not resume the abandoned interval.
A restored reading position is not a tail sample until the tail is shown.
Rapid composer input coalesces while awaiting paint. Each workflow retains at
most 100 measures, clearing its own buffer at the cap; live observers still
receive every completed sample. Starts without completion produce no measure.
Agent identifiers remain only in local matching state, never timing entry names
or details. These browser measures complement, rather than replace, the existing
UX trace spans and transport diagnostics.
