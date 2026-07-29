# Graceful Service Shutdown

## Problem

The daemon currently exits as soon as it receives Ctrl-C. This can terminate
reviews while their database state is `In Review`, discard partial work, and
cause the reviews to be reset to `Pending` on the next startup.

Operators need a drain mode with narrower behavior:

- stop starting reviews for `Pending` patchsets;
- allow reviews which have already started to finish;
- keep ingestion, API, email, patchwork, embargo release, cross-review, and
  metrics work operating normally while reviews drain; and
- exit the daemon once all locally started reviews are complete.

## Signal Semantics

SIGINT and SIGTERM both request graceful shutdown. SIGINT preserves the
existing Ctrl-C interface, while SIGTERM supports service managers and
container orchestrators.

The main task owns a Tokio watch channel. After receiving either signal, it
sets the channel value to `true` and waits for the reviewer service task to
finish. Other daemon tasks are not cancelled during this wait. They stop when
the Tokio runtime exits after the reviewer has drained.

## Review Scheduling Gate

The reviewer receives the watch channel. Before spawning each patchset review,
it borrows the current channel value and holds that borrow through the spawn.
A watch sender cannot publish the shutdown value while this borrow is held.
This makes the scheduling boundary linearizable:

- a review spawned while the borrow is held started before shutdown; or
- shutdown is visible and the review is not spawned.

Semaphore acquisition is interruptible by shutdown so a full review queue
does not prevent the reviewer loop from entering drain mode.

## Tracking and Draining Reviews

Patchset review tasks are stored in a Tokio `JoinSet` owned by the reviewer
loop. This tracks the actual lifetime of locally started work, including setup
which occurs before individual review records transition to `In Review`.

During normal operation, completed tasks are reaped and failures are logged.
During drain mode, the reviewer skips only pending-patchset scheduling. It
continues its other periodic work and waits for the `JoinSet` to become empty.
The reviewer then returns, allowing the main task to exit successfully.

Tracking tasks directly is preferable to polling database status because it
does not race with status transitions and cannot mistake setup work for an
idle service.

## Failure Behavior

A panic in a review task counts as completion and is logged. Existing review
error handling and database status transitions remain unchanged. There is no
graceful-shutdown timeout; deployment systems may use their existing forced
termination timeout if a review cannot finish.

## Testing

Unit tests cover the lifecycle control independently of AI providers:

- shutdown interrupts waiting for a review concurrency permit;
- no task is spawned after shutdown becomes visible; and
- drain completion waits until every tracked review task exits.

Normal formatting, lint, and unit test suites validate the main integration.
