# Feature Specification: File And Named Pipe APIs

**Branch**: feature/file-namedpipe-apis  |  **Created**: 2026-08-02  |  **Revision**: 8  |  **Status**: Draft
**Input Brief**: Add safe async File and NamedPipe APIs built on the existing IOCP infrastructure.

**Revision history**
- r1 — initial draft.
- r2 — addresses SpecReview-r1 (10 blocking). Decides the backend abstraction shape rather than
  deferring it (B-02, and r1's A-09 removed); makes thread-affinity achievable by construction
  (B-03); separates resolved-failure from dropped-future fates (B-04); adds an explicit
  result-and-error model section (B-05); decides access-direction handling (B-06); decides the
  pipe-name contract (B-07); completes the handle-adoption safety contract (B-08); names the
  numeric allocation budgets and the sizes they must hold across (B-09); states the byte-mode
  default (B-10); closes all traceability gaps (B-01); narrows the teardown criterion and
  rewords the outcome-matching requirement (NB-02, NB-03).
- r3 — addresses SpecReview-r2 (5 blocking). Makes teardown backend-specific instead of
  universal and redefines the submitter role so it describes both backends truthfully, since
  only the thread-pool backend has a per-handle registration token (B-01, B-02); narrows unsafe
  handle adoption to files and removes the pipe-security escape-hatch claim (B-03); replaces the
  unobservable no-copy requirement with observable buffer-identity and allocation budgets (B-04);
  adds a deterministic setup access-denied criterion (B-05); makes the read outcome a closed,
  exhaustively-matchable set (NB-01); scopes the source-level unsafe-impl check (NB-02).
- r4 — addresses SpecReview-r3 (3 blocking). Adds FR-012a: the handle is held through a shared
  reference-counted holder that every operation also references, so it is closed only after the
  last operation that could touch it is gone. This closes the stale-handle cancellation hole the
  review found — a late future-drop can no longer cancel through a closed or recycled handle —
  and removes close-ordering from the caller's obligations entirely (r3 B-02). Extends pipe
  coverage to both registrars (r3 B-01, SC-056) and adds SC-057/SC-058 for the late-cancellation
  and outliving-operation cases. Repairs stale narrative text that still asserted universal
  release-before-close (r3 B-03).
- r5 — addresses SpecReview-r4 (1 blocking, 2 non-blocking). Rewrites SC-058, which was
  self-contradictory: owner-drop cancels outstanding operations, so a held future cannot be
  required to "resolve normally"; it now requires an in-flight future to resolve to *any*
  documented result that is specifically not a closed- or invalid-handle failure (B-01).
  Clarifies that under FR-012a the thread-pool owner releases its handle reference rather than
  closing (NB-01), and states the holder must be thread-safe to preserve FR-007's `Send` bound
  (NB-02).
- r6 — addresses the multi-model Planning Documents Review. Adds FR-029a, because the existing
  completion paths discard the transferred byte count on any non-success status, which makes the
  truncated-message count of FR-026/FR-043/FR-055 unreachable as previously written (M-1). Bounds
  FR-012a's guarantee to operations the safe type itself creates, since the borrowed handle of
  FR-014 can outlive them (M-7/P-1). Names the caller-driven allocation constant (M-6). Adds
  FR-050a forbidding a blocking wait (M-2), FR-045a on typestate moves not cancelling their own
  operation (M-3), FR-013a on the proactor-reference obligation (S-4), and FR-045b on the
  ownership required to disconnect (S-7). Adds SC-059 and SC-060.
- r7 — addresses the Planning Documents Review re-review. Removes the built-in busy wait entirely:
  the crate is runtime-agnostic and owns no timer, and the re-review proved the proposed awaitable
  backoff cannot work across both backends — `WaitForHandle` is proactor-only, and an event-typed
  operation submitted through the thread pool raises a pending-I/O count no callback ever clears,
  deadlocking teardown. FR-050/FR-050a now make waiting the caller's responsibility with a
  documented retry pattern; FR-046 drops the busy-behaviour option; SC-031, SC-032, and SC-060 are
  restated accordingly. FR-014 additionally warns that owner-drop cancels *all* I/O on the handle,
  including operations the caller built from the borrowed handle.
- r8 — addresses final requirements audit finding SC-050. The allocation-budget integration test
  necessarily installs a counting global allocator, matching the existing `httpsys_alloc` harness
  precedent, so SC-050 now explicitly permits unsafe blocks that implement and delegate that
  allocator while keeping the adoption-test exception unchanged.

## Overview

`winasio` can already make any Windows overlapped API awaitable. Its `iocp` module provides
two completion backends and a small operation abstraction, and `httpsys` proves the design
works by building a complete HTTP.sys server on top of it. What the crate cannot do is the
most ordinary thing a Windows async library is expected to do: open a file or a named pipe
and read from it.

Everything needed to do that by hand exists, but every caller must assemble it themselves,
and the assembly is exactly where the hazards are. A handle must be opened with the right
flag or overlapped I/O silently degrades to blocking. It must be registered with a
completion mechanism exactly once, permanently. The registration must be torn down before
the handle is closed, or teardown blocks forever. A named pipe server must treat one
particular "error" from the connect call as success, or it drops every client that arrives a
microsecond early. None of this is discoverable, and all of it is `unsafe`.

This work adds two safe modules — one for files, one for named pipes — that make those
hazards structurally impossible rather than merely documented. A file is opened through a
builder that cannot forget the overlapped flag. A handle's registration is created once, by
construction, and torn down correctly for its backend because the type owns both — and the
handle itself outlives every operation that could still touch it, so no teardown ordering is
left to the caller. A pipe server is modelled as a typestate, so asking a not-yet-connected
pipe to read is a compile error rather than a runtime one. Reads report end-of-stream, a
closed peer, and a truncated message as ordinary outcomes to be matched on, not as errors to
be decoded.

Unlike `httpsys`, these types work with **either** completion backend. That requires a
public abstraction over "something a handle can be registered with", which the crate does not
have today and which this work introduces. The result is that a single-threaded caller
driving their own completion port and a caller who wants the system thread pool to do it use
the same file and pipe types, written the same way — and each gets exactly the thread-safety
their backend permits, derived automatically rather than asserted.

## Objectives

- Let a caller perform asynchronous file reads and writes without writing any `unsafe` code.
- Let a caller run a named pipe server and a named pipe client without writing any `unsafe`
  code, including correct handling of the connect race that catches most implementations.
- Make the three standing hazards of overlapped handle use — wrong open flags, double or
  missing registration, and closing a handle that outstanding operations still reference —
  unrepresentable in the safe API.
- Make both completion backends usable through one set of types, so choosing a backend is a
  one-line decision rather than an architectural one.
- Report expected terminal conditions (end of file, closed peer, truncated message) as
  values that the type system forces the caller to consider.
- Keep the crate's existing ownership contract intact: the caller's buffer is handed in and
  handed back, on success and on failure alike, with no hidden copies.
- Add exactly one allocation per single I/O operation with a caller-supplied buffer, and
  prove that number by measurement rather than assertion.

## User Scenarios & Testing

### User Story P1 – Asynchronous file read and write

**Narrative**: A developer wants to read a region of a file and write another region, from an
async context, without blocking a thread and without dropping into `unsafe`.

**Independent Test**: Open a temporary file through the crate's builder, write a known byte
pattern at a known offset, read it back at that offset, and assert the bytes match and the
supplied buffer was returned.

**Acceptance Scenarios**:
1. Given a path that does not exist, When the caller opens it with create-and-write enabled,
   Then the file is created and a value usable for asynchronous writes is returned.
2. Given an open file and an owned buffer, When the caller writes at offset N, Then the
   operation reports the number of bytes written and returns the buffer.
3. Given a file containing K bytes, When the caller reads at offset 0 into a buffer larger
   than K, Then the outcome reports K bytes and the buffer's readable length is K.
4. Given a file containing K bytes, When the caller reads at offset K, Then the outcome is
   end-of-file rather than an error or a zero-byte read.
5. Given a write that fails because the file was opened read-only, When the operation
   resolves, Then the error is reported *and* the buffer is still returned to the caller.
6. Given a thread-pool-backed open file, When it is dropped, Then its per-handle registration
   is released before its handle is closed, and the drop returns without blocking.
7. Given a caller-driven-backed open file with an operation in flight, When it is dropped,
   Then the drop returns promptly without the proactor being driven, the handle stays open
   until the operation's record is reclaimed, and reclamation occurs once the caller next
   drives the proactor.
8. Given a path that names an existing directory, When the caller opens it as a file, Then an
   error is returned rather than an unusable value.

### User Story P2 – Named pipe server accepting a client

**Narrative**: A developer wants to stand up a named pipe server, wait for a client, exchange
bytes, and then serve the next client on the same instance.

**Independent Test**: Create a server instance on a unique name, connect a client from the
same process, send a request and read a reply on both ends, disconnect, and confirm the
returned instance can accept a second client.

**Acceptance Scenarios**:
1. Given a created but unconnected server instance, When no client has arrived, Then the
   connect operation remains pending rather than completing or failing.
2. Given a created server instance, When a client connects before the server asks, Then the
   connect operation completes successfully rather than reporting an error.
3. Given a connected pipe, When the caller writes and the peer reads, Then the peer observes
   exactly the bytes written.
4. Given a connected pipe, When the caller disconnects, Then an unconnected server instance
   for the same name is returned and can accept another client.
5. Given a connected pipe whose peer has closed, When the caller reads, Then the outcome is
   a closed-peer condition rather than an error or a zero-byte read.
6. Given an unconnected server instance, When the caller attempts to read from it, Then the
   program does not compile.

### User Story P3 – Named pipe client connecting to a server

**Narrative**: A developer wants to connect to an existing named pipe as a client and
exchange bytes asynchronously.

**Independent Test**: With a server instance listening, connect a client through the crate's
client builder and complete a request/response exchange.

**Acceptance Scenarios**:
1. Given a listening server instance, When the client connects by name, Then a connected
   pipe supporting asynchronous reads and writes is returned.
2. Given a pipe whose instances are all busy, When the client connects with a wait timeout,
   Then it waits for an instance to free up and connects, rather than failing immediately.
3. Given a pipe whose instances are all busy, When the client connects with no wait
   configured, Then a busy condition is reported promptly and distinguishably.
4. Given a pipe name that does not exist, When the client connects, Then a not-found condition
   is reported, distinguishable from busy.

### User Story P4 – One API, either completion backend

**Narrative**: A developer running a single-threaded loop that drives their own completion
port, and a developer running a multi-threaded runtime who wants the system thread pool to
deliver completions, both want to use the same file and pipe types.

**Independent Test**: Run the same file round-trip test body twice — once against the
caller-driven backend, once against the thread-pool backend — through the same public types.

**Acceptance Scenarios**:
1. Given the caller-driven backend, When a file is opened against it, Then reads and writes
   complete when the caller drives the backend, using the same methods as any other backend.
2. Given the thread-pool backend, When a file is opened against it, Then reads and writes
   complete with no driving by the caller.
3. Given a file or pipe built on the thread-pool backend, When the caller moves it to
   another thread or shares it across threads, Then the program compiles.
4. Given a file or pipe built on the caller-driven backend, When the caller attempts to send
   it to another thread, Then the program does not compile.
5. Given any backend, When a handle is registered by opening a file or creating a pipe, Then
   no second registration of that handle is possible through the safe API.

### User Story P5 – Whole-payload convenience

**Narrative**: A developer does not want to hand-write the partial-read and partial-write
loops that every user of raw reads and writes otherwise duplicates.

**Independent Test**: Write a payload larger than a single operation is guaranteed to
transfer using the write-all helper, then read it back with the read-exact helper, and assert
equality and buffer return.

**Acceptance Scenarios**:
1. Given a buffer of N bytes, When the caller uses the write-all helper, Then all N bytes are
   transferred even if the underlying operation completes partially, and the buffer is
   returned.
2. Given a stream of unknown length, When the caller uses the read-to-end helper, Then the
   returned buffer holds every byte up to end-of-stream.
3. Given a read-exact request for N bytes on a stream that ends after M < N bytes, When the
   helper resolves, Then an unexpected-end condition is reported, the buffer is returned, and
   the reported transferred count is M.
4. Given any helper, When an underlying operation fails partway, Then the resolved value
   carries the buffer, the failure, and the count already transferred.

### User Story P6 – Message-mode named pipes

**Narrative**: A developer wants message framing from the operating system rather than
implementing their own, and needs to know when a read did not capture the whole message.

**Independent Test**: Create a message-mode server and client, send a message larger than the
reader's buffer, and confirm the reader is told the message was truncated and can retrieve
the remainder.

**Acceptance Scenarios**:
1. Given a message-mode pipe, When a message smaller than the buffer is read, Then the
   outcome reports exactly that message's length and does not include any part of the next
   message.
2. Given a message-mode pipe, When a message larger than the buffer is read, Then the outcome
   is a distinct truncated-message condition carrying the number of bytes delivered.
3. Given a truncated message, When the caller reads again, Then the remainder of the same
   message is delivered.
4. Given a byte-mode pipe, When a large payload is read into a small buffer, Then the outcome
   is an ordinary partial read, never a truncated-message condition.
5. Given a message-mode pipe, When a zero-length message is written, Then the reader observes
   a zero-length message rather than end-of-stream.
6. Given builders on which no mode was specified, When a large payload is read into a small
   buffer, Then the outcome is an ordinary partial read — that is, byte mode is the default.

### Edge Cases

- **Connect race**: a client that connects between pipe creation and the connect call must be
  accepted, and the connect operation must not wait for a completion that will never arrive.
- **Closed peer reported from the initiating call**: this condition is delivered
  synchronously rather than through a completion, so it must be classified on the initiating
  path as well as the completion path.
- **End of file reported through the completion**: this condition arrives asynchronously, so
  it must be classified on the completion path as well as the initiating path.
- **Zero-length read or write**: legal on both files and pipes; must not be confused with
  end-of-stream. In message mode a zero-length write is a real message.
- **Buffer with zero capacity**: a read into an empty buffer must not be undefined behaviour
  and must not be reported as end-of-stream.
- **Dropping an in-flight operation future**: inherited from the existing layer — cancellation
  is requested, the buffer is *not* returned, and its memory is retained until the completion
  arrives. Must be restated on the new API, not left to the reader to discover.
- **Dropping a helper future midway**: a helper is a sequence of operations, so dropping it is
  the same non-cancel-safe case as above. Bytes already transferred to or from the peer have
  genuinely been transferred and are not undone; the caller learns neither the count nor gets
  the buffer back. Must be documented on every helper.
- **Dropping a connected pipe without disconnecting**: must close cleanly, without blocking
  and without leaking the registration.
- **Reusing an instance after disconnect**: the returned unconnected instance must be
  genuinely reusable, not merely a value that fails on the next connect.
- **Two clients racing for one instance**: exactly one connects; the other observes the busy
  condition or waits, according to its configuration.
- **Operation contrary to the configured access direction**: e.g. writing to a read-only pipe.
  Reported as a failure when the operation resolves, not silently ignored.
- **Invalid pipe name**: empty, over-long, or containing a path separator or interior NUL —
  rejected before any platform call.
- **Registration failure after the handle is opened**: the handle must be closed before the
  error is returned; no handle leaks on this path.
- **Very large single transfer**: a transfer whose length exceeds what one platform call can
  express must be rejected explicitly or handled by the helpers, never truncated silently.

## Requirements

### Functional Requirements — Backend abstraction

The abstraction has **two roles**, because the two existing backends genuinely differ in
shape: a *registrar* turns a handle into a *submitter* for that handle, and a submitter accepts
operations. How each existing backend fills those roles is decided here, not in planning.

- FR-001: A public *registrar* abstraction shall exist whose single capability is: given a
  raw handle, register it with this completion mechanism and yield the *submitter* through
  which operations on that handle are issued. What the submitter owns is backend-specific and
  is decided in FR-003 and FR-004. (Stories: P4)
- FR-002: A public *submitter* abstraction shall exist whose single capability is: submit an
  operation and yield the crate's existing in-flight-operation future. (Stories: P4)
- FR-003: The system thread pool shall be represented as a registrar value carrying no state,
  since the thread pool needs nothing from the caller before a handle exists. Registering with
  it shall yield the crate's existing per-handle thread-pool registration as the submitter.
  This submitter **is** a releasable per-handle registration token. (Stories: P4)
- FR-004: The caller-driven completion port shall be represented as a registrar consisting of
  shared ownership of a proactor. Registering shall attach the handle to that proactor and
  yield another shared-ownership reference to the same proactor as the submitter. This
  submitter is **not** a per-handle token: the handle's association with the port ends only
  when the handle is closed, and the submitter can carry operations for any handle attached to
  the same proactor. Its purpose is to guarantee, structurally, that the proactor outlives
  every file and pipe registered against it, without a lifetime parameter on the safe types.
  (Stories: P4)
- FR-005: Safe file and pipe types shall be parameterised by the submitter type and shall own
  their submitter. (Stories: P1, P2, P3, P4)
- FR-006: Safe file and pipe types shall obtain their thread-affinity by automatic derivation
  from the submitter they own — that is, this work shall add **no** `unsafe impl Send` and no
  `unsafe impl Sync` to any new type. To make that possible, an owned handle shall be stored in
  the crate's existing thread-agnostic handle wrapper, whose unsafe assertions already exist
  and are already audited. (Stories: P4)
- FR-007: Submission through the submitter abstraction shall require operations to be sendable
  between threads, that being the stricter of the two backends' existing bounds. (Stories: P4)
- FR-008: The inherent methods of both existing backends shall keep their current signatures,
  bounds, and behaviour; in particular the caller-driven backend's own submit shall keep its
  looser bound for callers who use it directly. (Stories: P4)
- FR-009: Every test that exists in the repository before this work shall continue to pass
  without modification. (Stories: P4)

### Functional Requirements — Handle ownership and teardown

- FR-010: A safe file or pipe type shall own exactly one handle together with the submitter
  through which that handle's operations are issued. (Stories: P1, P2, P3)
- FR-011: **Thread-pool teardown.** Dropping a safe type whose submitter is a per-handle
  registration token (FR-003) shall drop that token — which cancels outstanding operations and
  waits for their callbacks to run — and only then release the safe owner's handle reference.
  The kernel handle is closed when the last shared holder reference is released (FR-012a).
  (Stories: P1, P2, P3)
- FR-012: **Caller-driven teardown.** Dropping a safe type whose submitter is shared ownership
  of a proactor (FR-004) shall request cancellation of outstanding operations on its handle and
  then release its own reference to the handle, without blocking. It shall not wait for
  completions, because doing so would require driving a proactor the caller owns. The operation
  records of cancelled operations are reclaimed when the proactor subsequently delivers their
  completions; FR-004 guarantees the proactor is still alive to do so. The safe type's
  documentation shall state that the caller must keep driving the proactor after such a drop
  until reclamation occurs. (Stories: P1, P4)
- FR-012a: **The handle outlives every operation that can still reference it.** An owned handle
  shall be held through a shared, **thread-safe** reference-counted holder — thread-safe because
  FR-007 requires submitted operations to be sendable between threads, and each operation holds
  a reference. The safe type and every operation it submits shall each hold a reference, and the
  handle shall be closed only when the last reference is released. Consequently, dropping a safe type never closes a handle that an
  outstanding operation could still use, and no cancellation — whether requested by the safe
  type's drop (FR-012) or by a later drop of an unresolved operation future (FR-034) — can ever
  target a closed or recycled handle. This guarantee covers operations the safe type itself
  creates. It does **not** extend to operations a caller constructs independently from the handle
  borrowed via FR-014: such an operation can outlive the safe type, and FR-014's documentation
  shall say so. Because the holder is allocated once when the handle is acquired and each
  operation merely takes a reference, this shall not affect the per-operation allocation budget of
  NFR-003. (Stories: P1, P2, P3, P4)
- FR-013: Dropping a safe type shall return without blocking on operations that can never
  complete, and shall not require the caller to drain anything beforehand. (Stories: P1, P2, P3)
- FR-013a: FR-013 assumes the caller retains their own reference to the caller-driven backend. If a
  safe type holds the *last* reference to the proactor, dropping it also shuts the proactor down,
  which drains and therefore blocks. The safe type's documentation shall state this obligation.
  (Stories: P1, P4)
- FR-014: A safe type shall expose its underlying handle for interoperation without
  transferring ownership. Its documentation shall state that an operation the caller builds from
  this handle is outside FR-012a's guarantee, must not outlive the safe type, and will additionally
  be cancelled by the safe type's own drop, which cancels all I/O on the handle. (Stories: P1, P2, P3)
- FR-015: A safe type shall be usable through a shared reference for all I/O, so concurrent
  operations from multiple tasks need no external synchronisation. (Stories: P1, P4)

### Functional Requirements — File

- FR-016: The crate shall provide a file-open builder configuring at minimum: read access,
  write access, create-if-missing, create-new (fail if exists), truncate-on-open, share mode,
  and additional platform flags and attributes. (Stories: P1)
- FR-017: The builder shall always request overlapped mode on the resulting handle and shall
  provide no way for a caller to disable it. (Stories: P1)
- FR-018: Opening a file shall register the resulting handle with the caller-supplied
  registrar as part of the open, with no separate registration step the caller can skip or
  repeat. (Stories: P1, P4)
- FR-019: If registration fails after the handle has been opened, the handle shall be closed
  before the error is returned. (Stories: P1)
- FR-020: The **file** module shall provide an `unsafe` constructor that adopts a
  caller-supplied handle and registers it. Its documented safety contract shall require **all**
  of: the handle is valid and currently owned by the caller; it was opened for overlapped I/O;
  it is not already registered with any completion mechanism; it has no outstanding overlapped
  operations owned by anything else; no other owner may close it and nothing else aliases it;
  and its access rights and object type are compatible with the role being constructed. The
  contract shall further state that on success, responsibility for closing the handle transfers
  to the returned value, and that on registration failure the constructor closes the handle
  before returning the error. Handle adoption for pipes is **not** provided, because a pipe's
  connection state, access direction, and byte/message mode cannot be recovered from the handle
  and would make the contract unverifiable. (Stories: P1)
- FR-021: The file type shall provide a read at an absolute offset, taking an owned mutable
  buffer and returning that buffer when the operation resolves, whatever the outcome.
  (Stories: P1)
- FR-022: The file type shall provide a write at an absolute offset, taking an owned buffer
  and returning that buffer when the operation resolves, whatever the outcome. (Stories: P1)
- FR-023: A successful read shall record the transferred length in the returned buffer, so the
  caller can read the bytes without separately tracking the count. (Stories: P1)
- FR-024: Reading at or past end of file shall be reported as the end-of-file outcome, not as
  an error and not as a zero-byte read. (Stories: P1)

### Functional Requirements — Result and error model

- FR-025: A read operation shall resolve to the crate's existing operation-result shape,
  carrying the caller's buffer plus either a *read outcome* or a platform error.
  (Stories: P1, P2, P3, P6)
- FR-026: The read outcome shall be a single closed set of alternatives, exhaustively
  matchable by the caller without a catch-all arm, comprising exactly: a transferred byte
  count; end-of-file; closed peer; and truncated message carrying the byte count delivered.
  (Stories: P1, P2, P6)
- FR-027: A caller shall be able to distinguish every alternative in FR-026 without inspecting,
  comparing, or decoding any platform error code. (Stories: P1, P2, P6)
- FR-028: Conditions other than those in FR-026 shall remain platform errors and shall not be
  folded into the read outcome. (Stories: P1, P2, P3)
- FR-029: The conditions in FR-026 shall be recognised whether the platform reports them from
  the initiating call or through the completion. (Stories: P1, P2, P6)
- FR-029a: The transferred byte count shall be preserved for a truncated message even though the
  platform reports that condition as a non-success status. The existing completion paths discard
  the count whenever the status is not success, so this requires an **additive** change to the
  operation-completion interface that carries the count alongside the result and defaults to
  today's behaviour, leaving every existing operation unaffected (FR-008, FR-064). Without this,
  the count required by FR-026, FR-043, and FR-055 is unreachable, `set_init` never runs for a
  truncated read (FR-023), and the caller cannot tell which bytes of their own buffer are valid.
  (Stories: P6)
- FR-030: A zero-byte transfer shall be reported as a zero-byte transfer, never as end-of-file,
  a closed peer, or a truncated message. (Stories: P1, P6)
- FR-031: A write operation shall resolve to the crate's existing operation-result shape,
  carrying the caller's buffer plus either the transferred byte count or a platform error.
  (Stories: P1, P2, P3)
- FR-032: Setup operations — open, create, connect — shall report failure through a documented
  set of categories distinguishing at minimum: not found; all instances busy; access denied;
  invalid name; already registered; and any other platform error. Each category shall be
  matchable by the caller without decoding a platform error code. (Stories: P1, P2, P3)
- FR-033: A helper (FR-058–FR-060) shall resolve to a single value carrying all three of: the
  caller's buffer; the number of bytes transferred before the helper stopped; and either
  success or a failure category distinguishing at minimum unexpected end of stream, closed
  peer, and any other platform error. (Stories: P5)
- FR-034: Every operation and helper future shall, when *dropped before resolving*, follow the
  existing layer's cancellation semantics: cancellation is requested, and neither the buffer
  nor the transferred count is returned to the caller. Cancellation shall be issued through the
  shared handle holder of FR-012a, so that it remains valid even if the safe type that created
  the operation has already been dropped. This is distinct from FR-021, FR-022, FR-031, and
  FR-033, which govern futures that resolve. Both behaviours shall be documented on the public
  API. (Stories: P1, P5)

### Functional Requirements — Pipe naming

- FR-035: Pipe builders shall accept a bare pipe name — the final component only — and shall
  themselves compose the local named-pipe path. Callers shall not pass a prefixed path.
  (Stories: P2, P3)
- FR-036: Pipe builders shall reject, before any platform call, a name that is empty, longer
  than the platform's limit for that component, or contains a path separator or an interior
  NUL. Rejection shall use the invalid-name category of FR-032. (Stories: P2, P3)
- FR-037: Remote pipe names shall be out of scope; the composed path shall always address the
  local machine. (Stories: P2, P3)

### Functional Requirements — Named pipe server

- FR-038: The crate shall provide a pipe-server builder configuring at minimum: pipe name,
  access direction, maximum instances, input and output buffer sizes, default timeout, byte or
  message type, and a first-instance restriction. (Stories: P2, P6)
- FR-039: The server builder shall default to byte mode; message mode shall be selected only by
  an explicit option. (Stories: P6)
- FR-040: The server builder shall always request overlapped mode and shall provide no way to
  disable it. (Stories: P2)
- FR-041: Creating a server instance shall register its handle with the caller-supplied
  registrar as part of creation. (Stories: P2, P4)
- FR-042: An unconnected server instance shall expose an asynchronous connect operation that
  consumes it and yields a connected pipe. (Stories: P2)
- FR-043: An unconnected server instance shall expose no read, write, or helper operation, such
  that attempting one is a compile-time error. (Stories: P2)
- FR-044: A client that connects between instance creation and the connect call shall be
  accepted as a successful connection, without waiting for a completion. (Stories: P2)
- FR-045: A connected pipe shall provide a disconnect operation yielding an unconnected server
  instance for the same handle, reusable for a subsequent client. (Stories: P2)
- FR-045a: A typestate transition that consumes a value — connect (FR-042) and disconnect (FR-045)
  — shall not cancel the operation it is itself initiating. Owner-drop cancellation exists for
  abandonment, not for a value being moved into its successor state. (Stories: P2)
- FR-045b: Because disconnect consumes the connected pipe, it requires exclusive ownership and is
  therefore unavailable to a caller who has shared the pipe for concurrent I/O under FR-015. The
  documentation shall state this and describe the ownership pattern that supports both.
  (Stories: P2)

### Functional Requirements — Named pipe client

- FR-046: The crate shall provide a pipe-client builder configuring at minimum: pipe name,
  access direction, and read mode (byte or message). (Stories: P3, P6)
- FR-047: The client builder shall default to byte read mode. (Stories: P6)
- FR-048: The client builder shall always request overlapped mode. (Stories: P3)
- FR-049: Connecting as a client shall register the handle with the caller-supplied registrar
  as part of connecting. (Stories: P3, P4)
- FR-050: When all instances are busy, connecting shall report the busy category of FR-032
  promptly and shall not block the calling thread. (Stories: P3)
- FR-050a: The crate shall provide **no** built-in wait-and-retry for a busy pipe. It is
  runtime-agnostic and owns no timer, so the only ways to wait would be a synchronous platform call
  that stalls the caller's executor, or a timer abstraction tied to one completion backend —
  neither acceptable. Waiting is therefore the caller's responsibility, using their own runtime's
  timer, and the documentation shall show the retry pattern. (Stories: P3)
- FR-051: A client requesting message read mode shall have that mode applied to its handle as
  part of connecting, with no separate caller step. (Stories: P3, P6)

### Functional Requirements — Pipe I/O, access direction, and message mode

- FR-052: Connected pipes — server-side and client-side alike — shall provide a read and a
  write taking no offset, on the same buffer-ownership terms as file operations, plus the same
  helper surface. (Stories: P2, P3)
- FR-053: Read and write shall be available on every connected pipe regardless of the
  configured access direction; an operation contrary to that direction shall resolve as a
  platform access failure, returning the caller's buffer, and this shall be documented. Access
  direction shall **not** be encoded in the type system. (Stories: P2, P3)
- FR-054: On a byte-mode pipe, a read into a buffer smaller than the available data shall be
  reported as an ordinary partial transfer, never as a truncated message. (Stories: P2, P6)
- FR-055: On a message-mode pipe, a read into a buffer smaller than the pending message shall
  be reported as a truncated message carrying the delivered byte count, with the remainder
  retrievable by a subsequent read. (Stories: P6)
- FR-056: The crate shall not grow buffers or retry automatically on a truncated message; the
  caller decides. (Stories: P6)
- FR-057: A read on a pipe whose peer has closed shall be reported as the closed-peer outcome.
  (Stories: P2, P3)

### Functional Requirements — Convenience helpers

- FR-058: The crate shall provide a write-all helper transferring an entire buffer across as
  many operations as required. (Stories: P5)
- FR-059: The crate shall provide a read-exact helper filling a requested length across as many
  operations as required. (Stories: P5)
- FR-060: The crate shall provide a read-to-end helper reading until end-of-stream.
  (Stories: P5)
- FR-061: A read-exact helper encountering end-of-stream before the requested length shall
  report the unexpected-end category of FR-033 rather than succeeding short. (Stories: P5)
- FR-062: The write-all and read-exact helpers shall operate in place on the caller's buffer
  and shall return that same allocation — same address and same capacity — rather than a
  substitute. The read-to-end helper shall allocate only its per-operation records and the
  growth of its accumulating buffer. These are stated as observable budgets rather than as a
  prohibition on copying, so that they can fail a test. (Stories: P5)

### Functional Requirements — Additivity

- FR-063: The existing low-level operation types shall remain public and directly usable, so
  the safe layer is additive rather than a replacement. (Stories: P4)
- FR-064: No existing public API shall be removed, renamed, or have its behaviour changed.
  (Stories: P4)

### Key Entities

- **Registrar**: a completion mechanism a handle can be registered with; produces a submitter.
- **Submitter**: the value a safe file or pipe owns that is sufficient to issue operations for
  its registered handle, and whose thread-affinity determines that of the owning safe type. For
  the thread-pool backend it is a releasable per-handle registration token; for the
  caller-driven backend it is shared ownership of the proactor, which is not per-handle.
- **File**: an owned overlapped file handle plus its submitter, supporting positional I/O.
- **Open options**: the builder that produces a File.
- **Server options / Client options**: the builders producing, respectively, an unconnected
  pipe instance and a connected client pipe.
- **Unconnected pipe instance**: a created-but-not-yet-connected server pipe; can only connect.
- **Connected pipe**: a pipe with a peer; supports read, write, helpers, and disconnect.
- **Read outcome**: the closed set of alternatives of FR-026.
- **Setup error category / helper failure category**: the closed sets of FR-032 and FR-033.

### Cross-Cutting / Non-Functional

- NFR-001: The safe API shall require no `unsafe` from callers for any operation in stories
  P1–P6, with the sole exception of the handle-adopting constructor (FR-020).
- NFR-002: Every `unsafe` block added by this work shall carry a `SAFETY:` comment naming the
  invariant it relies on, and every `unsafe fn` a `# Safety` section, matching the crate's
  existing convention.
- NFR-003: A single file read, file write, pipe read, or pipe write with a caller-supplied
  buffer, on the **thread-pool** backend, shall allocate **exactly once** — the operation record
  itself. This budget shall hold for buffer capacities of at least 64 bytes, 4096 bytes, and
  1 MiB, and for transfers that are empty, partial, and buffer-filling.
- NFR-004: The budget in NFR-003 is a merge gate, not a description: an implementation
  measuring higher fails, and the budget may be raised only by revising this spec with a stated
  reason. The caller-driven backend is excluded from the exact-count assertion because its
  pending-operation bookkeeping amortises allocation; it shall instead be asserted not to grow
  per-operation without bound.
- NFR-005: The work shall add a benchmark covering at least a file read/write round trip and a
  pipe request/response round trip.
- NFR-006: The compile-time guarantees this spec claims — that an unconnected pipe cannot read
  (FR-043), and that a caller-driven-backend type cannot be sent across threads (FR-006) —
  shall each be proven by a test that fails to compile.
- NFR-007: No test shall depend on a wall-clock timeout for its *success* path; timeouts may
  exist only as failure bounds, so that a hang becomes a failure rather than a stall.
- NFR-008: Tests using named pipes shall derive names unique per test binary and per test, so
  parallel execution cannot cause cross-test interference.
- NFR-009: The repository's existing continuous integration checks — formatting, clippy at
  deny-warnings across all targets and features, the full test suite, doctests, and the
  ignored-test soak — shall pass unchanged.
- NFR-010: Public items shall carry rustdoc; the two new modules shall each carry module-level
  documentation stating their invariants and the caller's obligations, matching the style of
  the existing HTTP.sys module.

## Success Criteria

### Backend abstraction and thread affinity

- SC-001: The same file round-trip test body passes against both backends through identical
  public types, with only the registrar differing at the call site. (FR-001, FR-002, FR-003,
  FR-004, FR-005)
- SC-001a: The per-operation allocation budget of NFR-003 is unaffected by the shared handle
  holder, asserted by the same measurement that establishes the budget. (FR-012a, NFR-003)
- SC-002: A file and a pipe built on the thread-pool backend compile when moved to another
  thread and when shared across threads. (FR-005, FR-006)
- SC-003: A **`File` built on the caller-driven backend** fails to compile when sent to another
  thread, proven by a compile-fail test on the real public type rather than on a stand-in.
  (FR-006, NFR-006)
- SC-004: A repository test scans the source files added by this work for `unsafe impl Send`
  and `unsafe impl Sync` and finds none; pre-existing wrappers outside those files are excluded
  from the scan. SC-002 and SC-003 remain the behavioural proof. (FR-006, NFR-002)
- SC-005: An operation that is not sendable between threads fails to compile when submitted
  through the submitter abstraction, while the caller-driven backend's inherent submit still
  accepts it. (FR-007, FR-008)
- SC-006: Every test present before this work passes unmodified. (FR-009, FR-063, FR-064)
- SC-007: The public surface of both existing backends is unchanged, evidenced by pre-existing
  backend tests compiling without edits. (FR-008, FR-064)

### Handle ownership and teardown

- SC-008: Dropping a **thread-pool-backed** file or pipe with an operation still in flight
  returns within a bounded time and the crate's live-operation counter returns to its pre-test
  baseline. This observes operation-record reclamation and non-blocking drop; it is not a proof
  of handle-leak absence. (FR-010, FR-011, FR-013, NFR-007)
- SC-053: Dropping a **caller-driven-backed** file with an operation still in flight returns
  promptly without the proactor being driven; then, after the caller drives the proactor, the
  live-operation counter returns to its pre-test baseline within a bounded number of poll
  iterations. The test states explicitly whether the operation future is held or dropped, and
  covers the held case. (FR-012, FR-013, FR-004)
- SC-056: A pipe server/client request-response exchange, and a pipe teardown with an operation
  in flight, both run through the same public pipe types against **both** registrars, with only
  the registrar differing at the call site. (FR-004, FR-005, FR-041, FR-049, FR-052, FR-012)
- SC-057: A test drops a caller-driven-backed file or pipe, then drops the still-unresolved
  operation future before driving the proactor, and observes bounded reclamation with no error
  and no effect on an unrelated handle opened afterwards — demonstrating that the late
  cancellation did not act on a closed or recycled handle. (FR-012a, FR-034)
- SC-058: A test holds an **in-flight** operation future past the drop of the file that created
  it, drives the completion to delivery, then polls the future and observes a documented result
  — either success or the documented cancellation/platform error — and specifically **not** a
  closed-handle or invalid-handle failure. The live-operation counter returns to baseline once
  the future resolves or is dropped. (FR-012a, FR-034)
- SC-009: A construction whose registration fails after the handle was opened leaves no open
  handle, verified by inducing registration failure and observing the handle is closed.
  (FR-019, FR-041, FR-049)
- SC-010: A file exposes its handle, and an operation issued through the low-level layer using
  that handle succeeds, confirming ownership was not transferred. (FR-014, FR-063)
- SC-011: Two concurrent positional reads issued through shared references to one file both
  complete correctly, with no external synchronisation in the test. (FR-015)
- SC-012: A file adopted through the unsafe constructor performs a successful read; the
  constructor's rustdoc enumerates every obligation listed in FR-020; no pipe type exposes a
  handle-adopting constructor; and no safe constructor anywhere in the new modules accepts a raw
  handle. (FR-020)

### File

- SC-013: A caller opens a file, writes a payload, and reads it back with matching bytes,
  writing no `unsafe` code. (FR-016, FR-017, FR-018, FR-021, FR-022, NFR-001)
- SC-014: Each builder option in FR-016 is exercised by at least one test asserting its
  observable effect — notably that create-new fails on an existing path and truncate empties
  one. (FR-016)
- SC-015: A successful read leaves the returned buffer's readable length equal to the
  transferred count, asserted without the test tracking the count separately. (FR-023)
- SC-016: Reading at end of file, and past it, both yield the end-of-file outcome — a condition
  the platform delivers through the completion. (FR-024, FR-025, FR-029)
- SC-017: Opening an existing directory as a file returns an error. (FR-016, FR-032)

### Result and error model

- SC-018: A failed write returns the error together with the caller's buffer. (FR-022, FR-031)
- SC-019: Every alternative of the read outcome is produced by at least one test and matched
  without the test comparing any platform error code. (FR-025, FR-026, FR-027, FR-028)
- SC-020: A zero-byte read and a zero-byte write are each reported as a zero-byte transfer.
  (FR-030)
- SC-021: Each setup failure category of FR-032 that can be provoked — not found, busy, invalid
  name, already registered — is produced by a test and matched without decoding a platform error
  code. (FR-032, FR-036)
- SC-054: The setup access-denied category is provoked deterministically and without depending
  on process privileges — by connecting a client requesting an access direction the server
  instance does not grant — and is matched without decoding a platform error code, and is
  distinguishable from busy and not-found. (FR-032, FR-046, FR-050)
- SC-022: A test drops an in-flight operation future and asserts the documented consequence: the
  buffer is not returned, and the live-operation counter still returns to baseline once the
  completion arrives. (FR-034)

### Named pipe server

- SC-023: A pipe server built with each option of FR-038 creates successfully, and
  maximum-instances and the first-instance restriction each produce their documented failure
  when violated. (FR-038, FR-040, FR-041)
- SC-024: With no mode specified, an oversized read on the resulting pipe yields an ordinary
  partial transfer, demonstrating the byte-mode default. (FR-039, FR-054)
- SC-025: Attempting to read from an unconnected pipe instance fails to compile, proven by a
  compile-fail test. (FR-043, NFR-006)
- SC-026: A pipe server accepts a client that connected before the server called connect, in a
  test that deterministically produces that ordering. (FR-042, FR-044)
- SC-027: A pipe instance serves two clients in sequence through disconnect-and-reuse, and the
  connect and disconnect transitions do not cancel the operations they initiate (FR-045, FR-045a)
- SC-028: A read on a pipe whose peer has closed yields the closed-peer outcome — a condition
  the platform delivers from the initiating call. (FR-029, FR-057)
- SC-029: Dropping a thread-pool-backed connected pipe without disconnecting, and dropping an
  unconnected instance, both return promptly and leave the live-operation counter at baseline.
  (FR-011, FR-013)

### Named pipe client

- SC-030: A client connects to a listening server and completes a request/response exchange.
  (FR-046, FR-048, FR-049, FR-052)
- SC-031: A caller-driven retry loop — the caller waiting with its own timer, not the crate —
  connects successfully once the client initially holding the only instance releases it. (FR-050,
  FR-050a)
- SC-032: Connecting to a fully busy pipe reports the busy category, distinguishably from the
  not-found category produced by connecting to an absent name. (FR-032, FR-050)
- SC-033: A client requesting message read mode receives an oversized message as a truncated
  message, with no separate mode-setting call anywhere in the test. (FR-051, FR-055)
- SC-034: With no read mode specified, a client's oversized read yields an ordinary partial
  transfer. (FR-047, FR-054)
- SC-035: Names that are empty, over-long, separator-bearing, or NUL-bearing are each rejected
  with the invalid-name category before any platform call; a bare name works on both server and
  client, and the two interoperate. (FR-035, FR-036, FR-037)

### Pipe I/O and message mode

- SC-036: A write on a read-only pipe resolves as an access failure and returns the buffer.
  (FR-053)
- SC-037: A message-mode read of an oversized message yields the truncated-message outcome with
  a byte count, and a subsequent read retrieves the remainder, with the crate performing no
  automatic growth or retry. The reported count is asserted to equal the bytes actually delivered,
  and the returned buffer's readable length matches it. (FR-029a, FR-055, FR-056)
- SC-059: Two clients race for a single free instance; exactly one connects, and the other either
  observes the busy category or waits and then connects, according to its configuration. (FR-050)
- SC-060: Connecting to a busy pipe returns within a bounded time on both backends, and a source
  scan confirms the connect path contains no synchronous platform wait call — no built-in wait
  exists to block on. (FR-050, FR-050a)
- SC-038: A byte-mode read of the same oversized payload yields an ordinary partial transfer.
  (FR-054)
- SC-039: A zero-length message is observed by the reader as a zero-length message and not as
  end-of-stream. (FR-030, FR-055)

### Helpers

- SC-040: The write-all helper transfers a payload larger than one operation and returns the
  buffer. (FR-033, FR-058)
- SC-041: The read-to-end helper returns every byte of a multi-operation stream. (FR-060)
- SC-042: The read-exact helper reports unexpected-end on a short stream, returns the buffer,
  and reports a transferred count equal to the bytes actually read. (FR-033, FR-059, FR-061)
- SC-043: A helper failing partway returns the buffer, the failure category, and a non-zero
  transferred count. (FR-033)
- SC-044: The read-to-end helper's measured allocation count equals the number of operations it
  performed plus the reallocations caused by accumulator growth, and no more. (FR-062)
- SC-055: The write-all and read-exact helpers return a buffer whose address and capacity are
  identical to those of the buffer handed in, asserted in a test that records both before
  submission. (FR-062, FR-058, FR-059)

### Performance and quality gates

- SC-045: The measured allocation count for a single file read, file write, pipe read, and pipe
  write with a caller-supplied buffer on the thread-pool backend is exactly 1 in each case,
  asserted for buffer capacities of 64 bytes, 4096 bytes, and 1 MiB. (NFR-003, NFR-004)
- SC-046: That count is invariant across empty, partial, and buffer-filling transfers, and the
  caller-driven backend's per-operation count is asserted to be **at most 2** once warmed — the
  operation record plus at most one amortised insertion into the proactor's pending bookkeeping —
  rather than growing with operation number. That constant is recorded in the module rustdoc and in
  `Docs.md`. (NFR-003, NFR-004)
- SC-047: A benchmark exists and runs for a file round trip and a pipe round trip. (NFR-005)
- SC-048: Formatting, clippy at deny-warnings, the full test suite, doctests, and the ignored
  soak all pass. (NFR-009)
- SC-049: Every public item added has rustdoc; both new modules have module-level documentation
  covering invariants and caller obligations, including the dropped-future behaviour of FR-034
  on every operation and helper, the bounded scope of FR-012a's guarantee (FR-014), the
  proactor-retention obligation (FR-013a), and the ownership required to disconnect (FR-045b).
  (FR-013a, FR-014, FR-034, FR-045b, NFR-010)
- SC-050: No example, test, or benchmark added by this work contains an `unsafe` block, except
  where it exists specifically to exercise the handle-adopting constructor or to implement and
  delegate the counting global allocator used by allocation-budget tests. (FR-020, NFR-001,
  NFR-003)
- SC-051: Every named-pipe test derives its name from the test binary and test name, verified by
  a shared helper that all such tests use. (NFR-008)
- SC-052: The two new modules are documented in the repository README alongside the existing
  module sections. (NFR-010)

## Assumptions

- **A-01**: End-of-file is reported by the platform as a distinct condition delivered through
  the completion; a closed pipe peer is reported as a distinct condition delivered from the
  initiating call. Both were measured on the target platform during research (SpecResearch Q21,
  Q22). FR-029 deliberately does not depend on which path either arrives on, because neither
  delivery mode is contractually guaranteed across platform versions.
- **A-02**: The connect-race condition is reported by the platform as a specific
  already-connected condition and must be treated as success; measured during research
  (SpecResearch Q23) and already handled by an existing test in the repository.
- **A-03**: Requiring submitted operations to be sendable between threads (FR-007) excludes no
  operation that exists in the repository today; research confirmed every current operation
  satisfies it. Callers needing the looser bound retain the caller-driven backend's inherent
  method (FR-008).
- **A-04**: The named-pipe platform APIs are gated behind a bindings-crate feature the library
  does not currently enable; enabling it is required and acceptable. Verified by compile probe
  during research (SpecResearch Q14).
- **A-05**: The existing allocation-measurement harness is reusable, subject to living in a test
  binary that installs the counting allocator and keeping unrelated work outside the measured
  region. It counts allocations, not deallocations, on the measuring thread only.
- **A-06**: The single allocation of NFR-003 is the operation record the existing layer creates
  per submission; research measured the comparable HTTP.sys budget as three allocations for
  three operation records plus one buffer, consistent with one record per operation. If
  measurement contradicts this, NFR-004 requires revising this spec rather than the test.
- **A-07**: File metadata — size, timestamps, attributes — is not needed by any story here; the
  read-to-end helper discovers length by reading to end-of-file rather than querying size.
- **A-08**: A default security descriptor is acceptable for created pipes. Custom pipe security
  descriptors are out of scope entirely — there is no handle-adoption escape hatch for pipes
  (FR-020) — and are recorded as a phase candidate should a caller need them.
- **A-09**: "As many operations as required" in the helpers means a bounded loop driven by the
  transferred counts. No story requires a helper to be cancel-safe in the sense of resuming
  after its future is dropped; FR-034 documents that it is not.
- **A-10**: The existing read operation forms a mutable slice over a buffer's full capacity,
  including bytes never written. This is pre-existing in the crate. This spec does not require
  fixing it, but requires planning to decide deliberately whether the new code inherits, avoids,
  or isolates the pattern, and to record that decision.

## Scope

**In Scope**:
- A two-role public abstraction over completion backends (registrar and submitter), satisfied by
  both existing backends.
- New low-level operations required by the above: offsetless read and write, pipe connect.
- A safe file module: open builder, handle adoption, positional read/write, helpers, teardown.
- A safe named pipe module: server builder, typestate connect/disconnect/reuse, client builder
  with busy handling, byte and message modes, read/write, helpers, teardown, name validation.
- Read outcome classification for end-of-file, closed peer, and truncated message; setup and
  helper failure categories.
- Integration tests, allocation-budget assertions, compile-fail proofs, and benchmarks.
- Module documentation and a README section.
- Enabling the bindings-crate feature required for named pipes.

**Out of Scope**:
- Directory operations, metadata queries, timestamps, attributes, file locking, sparse files.
- Scatter/gather transfers.
- Anonymous pipes.
- Remote (non-local) pipe names.
- Device control, transactions, reparse points, alternate data streams.
- Pipe security descriptors and access-control configuration beyond the platform default, and
  handle adoption for pipes of any kind (FR-020 provides it for files only).
- Client impersonation on pipes.
- Encoding pipe access direction in the type system (FR-053 decides against it).
- Adapters implementing any external ecosystem's asynchronous read/write traits.
- A stateful file cursor, sequential read/write, or append mode.
- Buffer pooling or reuse across operations.
- Cancel-safe or resumable helpers.
- Any change to the existing HTTP.sys module.
- Sockets.

## Dependencies

- The existing `iocp` module: operation abstraction, both backends, buffer traits, the
  operation-result type, the thread-agnostic handle wrapper required by FR-006, and the
  live-operation counter used by teardown tests.
- The platform bindings crate, with an additional feature enabled for named pipe APIs (A-04).
- The existing test crate's harness conventions, allocation-counting allocator, and benchmark
  wiring.
- The repository's continuous integration configuration, unchanged.

## Risks & Mitigations

- **The chosen backend abstraction does not fit both backends in practice**: FR-001–FR-004
  decide a shape on paper, but only compilation proves it. *Mitigation*: implement and validate
  the abstraction **first**, in its own phase, with SC-001 and SC-005 passing against both
  backends, before any file or pipe code depends on it.
- **Type-parameter contagion**: parameterising every public type by the submitter affects every
  signature, doc example, and test, and may produce poor inference or error messages.
  *Mitigation*: write realistic doctests early; if inference proves painful, add type aliases for
  the common backend rather than abandoning genericity.
- **Pipe tests hang instead of failing**: a connect that is never answered blocks forever, and a
  hung test is far worse in continuous integration than a failing one. *Mitigation*: NFR-007 and
  NFR-008 — failure-bounded timeouts and per-test unique names, with both ends driven from one
  test process.
- **Inline versus completion delivery differs by condition and platform version**: research
  measured one delivery mode per condition on one machine. *Mitigation*: FR-029 requires
  classifying on both paths; SC-016 and SC-028 together cover one condition on each path.
- **Reading uninitialised buffer bytes**: A-10. *Mitigation*: planning must record an explicit
  decision rather than propagating the pattern silently.
- **The allocation budget turns out to be wrong**: NFR-003 names 1, derived from the structure of
  the existing layer rather than from measurement of this code. *Mitigation*: NFR-004 makes the
  response explicit — revise the spec with a reason, never relax the test quietly.
- **Scope creep into a general file API**: file work attracts metadata, directories, and locking.
  *Mitigation*: the out-of-scope list is explicit; anything beyond it becomes a phase candidate
  rather than an implementation.

## References

- Work shaping: `.paw/work/file-namedpipe-apis/WorkShaping.md`
- Research: `.paw/work/file-namedpipe-apis/SpecResearch.md`
- Research questions: `.paw/work/file-namedpipe-apis/ResearchQuestions.md`
- Review round 1: `.paw/work/file-namedpipe-apis/reviews/SpecReview-r1.md`
- Prior art in this repository: `crates/winasio/src/httpsys/` (safe wrapper precedent),
  `crates/winasio-tests/tests/extensibility.rs` (existing external named-pipe operation).
