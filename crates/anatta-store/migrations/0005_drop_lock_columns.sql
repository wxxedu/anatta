-- Per-conversation lock state moved to anatta-runtime's flock-based
-- SessionLock (under <anatta_home>/runtime-locks/). The OS releases
-- the lock the moment the holding process exits, so no DB-side
-- bookkeeping is needed and no stale-lock recovery is possible.
--
-- Dropping these columns also retires the migration-0004 design
-- (lock_holder_started_at + PID-reuse defense) and the legacy
-- migration-0003 lock_holder_pid column.

ALTER TABLE conversations DROP COLUMN lock_holder_pid;
ALTER TABLE conversations DROP COLUMN lock_holder_started_at;
