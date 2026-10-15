# Universal Research Agenda

Version- and project-agnostic questions the notes explorer answers about a
component by reading its code. They name no functions, files or versions.
Each `##` section is one topic, explored in its own stage. Answer with
facts that hold in the code at the pinned revision, each backed by code
evidence; a question that does not apply is marked as such, never padded.

## STATE: State, Invariants and Data Flow

- STATE-1: What are the central data structures and objects, which part of the code owns each, and which invariants must hold between their fields at all times?
- STATE-2: Which state transitions exist (lifecycles, modes, statuses), what triggers each, and which transitions must never happen?
- STATE-3: Where is the same fact stored in more than one place (caches, copies, derived values, persisted records), and what keeps the copies consistent?
- STATE-4: How does data flow from inputs to outputs, and at which points is it validated, transformed or persisted?

## SYNC: Concurrency and Synchronization

- SYNC-1: Which state is shared between concurrent contexts (threads, tasks, callbacks, processes), and what protects each piece?
- SYNC-2: What lock or ordering hierarchy exists, and which operations must never run while a given lock is held (blocking calls, I/O, waiting for another lock or task)?
- SYNC-3: Which operations must be atomic as a group, and where is a check followed by an action on state that can change in between?
- SYNC-4: Which execution contexts forbid blocking or long-running work, and how is such work moved elsewhere?
- SYNC-5: Where is work retried, replayed or delivered more than once, and what makes it idempotent?

## LIFE: Ownership, Lifetimes and Teardown

- LIFE-1: Who owns each resource (memory, handles, connections, files, processes, background jobs) at each stage, and how is ownership transferred or shared?
- LIFE-2: In which order must objects be created and published, and what prevents a partially constructed object from becoming visible to other code?
- LIFE-3: In which order must objects and background work be stopped and released, and what guarantees nothing still uses them afterwards?
- LIFE-4: What happens to in-flight work when its owner is cancelled, times out, restarts or shuts down?

## ERR: Error Handling and Failure Modes

- ERR-1: How are errors signaled and propagated, and where are they deliberately swallowed, downgraded or retried?
- ERR-2: On each failure path, what must be rolled back or released so the system returns to a consistent state?
- ERR-3: Which failures are expected in normal operation and which are fatal, and how does the code tell them apart?
- ERR-4: Where can a failure become silent, producing a plausible but wrong result instead of an error?

## API: Contracts and Boundaries

- API-1: What do the main interfaces promise their callers (preconditions, return conventions, ownership of results, required context)?
- API-2: Which contracts are implicit, enforced only by convention, naming or comments, and therefore easy to break?
- API-3: Which inputs cross a trust boundary, where are they validated, and what limits bound their size, count and cost?
- API-4: Which external interfaces (file formats, wire protocols, stored schemas, configuration keys, command-line behavior) must stay compatible, and what would break existing users or data?

## BOUND: Resource Bounds and Scaling

- BOUND-1: Which operations scale with input size, history length or fan-out, and what bounds each?
- BOUND-2: Where can caches, queues, buffers, retries or recursion grow without limit?
