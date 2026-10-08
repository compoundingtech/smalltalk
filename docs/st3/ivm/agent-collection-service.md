# Native agent collection Service

`Store::open_with_agent_collections(path, receiver)` opens an explicit agent source
installation with the Store's shared `Arc<Views>` and `Arc<Installer>`. All sockets
and receipt consumers share the serialized Store-owned `Arc<events::Publisher>`.
The production bridge uses that exact publisher and subscribes before its
authorized snapshot. `maintain_agent_collections()` executes one bounded page;
its boolean requests immediate continuation only when finite work progressed.

The daemon opts in through `ST3_AGENT_COLLECTION_IVM=1`. The `agents` entry then
uses the existing `st3.client.collections.v0` socket and complete public agent
rows. Ordinary `Store::open` retains the existing route. Other collection entries
are not certified by the agent source and keep their existing readers.

Installation scans the complete, receiver-bound physical capture manifest using
stable primary-key pages of at most 128 rows and 1 MiB. Paired transaction hooks
capture durable and local OLD/NEW replacements in the same source transaction.
A managed transaction supplies at most one captured maintenance clock; queued
Kernel work shares its aggregate 128-input budget. Missing, unsupported or
uncaptured dependencies refuse reads rather than falling back to a full fold.

Publication requires an actual Installer namespace, complete source scan/journal
continuity, closed canonical/local/authority/card dependencies, current native
projection coverage, captured deadlines and acknowledged whole-source producer
and file evidence. Source revisions and graph positions are separate identities.
An Installer root or an available registry flag alone never certifies a read.
Producer acknowledgment follows successful durable publication; maintenance and
the existing socket retry path observe that acknowledgment without another
Publisher.

Public row changes retain at most 1,024 distinct IDs per namespace. The next
distinct ID requests a bounded window refresh; duplicate IDs do not consume more
capacity. Journal acknowledgment shares the transaction that synchronizes Views
and persists the producer boundary. Rollback retains both. Namespace cleanup
shares one deletion budget with the Kernel reclaimer.

Maintenance never ticks a permanently pending source repeatedly. Private indexed
queue acknowledgments and finite cursor advances measure scheduling progress;
they grant no coverage or readiness. After a page makes no progress, an unchanged
SourcePosition waits for new captured input or a newly due deadline. Staging
catch-up can apply a previously queued clock after that wait starts; genuine
acknowledgments and net forward seeks during catch-up permit another finite page.
Cursor advance followed by a wrap inside one callback does not count as progress.
An idle, current boundary performs no maintenance write.

Local real Store controls cover publication, writes, complete public rows, ranked
socket deltas, reconnect, paired authority and changed-value raw recovery. A real
isolated daemon and stui also pass initial seeding, later writes, restart/reconnect
and idle silence. Managed checkpoint writers and deferred native projection are
included in that daemon. Current-cut readiness is briefly revoked after a write
until affected dependencies and producer evidence close; delivered rows stay stale
during that interval. Intended populated-source limits, physical device evidence,
deployed callback profiling and hosted review/checks remain separate gates.

Compatible reopening preserves the source fingerprint, epoch and revision. A
capture gap or fenced view requires actual native projection replay followed by
bounded extraction into a fresh namespace; synchronizing the retained root cannot
clear that fence. Recovery discards only the fixed, bounded pre-recovery capture
range that the new baseline covers. Interrupted installations reclaim unattached
namespaces and prune journal pages before restarting. A changed receiver, schema,
fingerprint or epoch keeps the agent source unavailable while the native Store
continues to serve. These recovery controls are separate from initial publication.
