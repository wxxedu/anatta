-- PID-reuse defense for the chat lock.
--
-- `lock_holder_pid` alone is insufficient across reboots and in
-- adversarial PID-recycling scenarios: a leftover lock pid may alias
-- an unrelated live process and falsely look held.
--
-- `lock_holder_started_at` records the holder process's start time
-- (Unix epoch seconds, as reported by sysinfo). Acquire writes it
-- alongside the pid; liveness check compares the recorded start time
-- against the current process at that pid. Mismatch = PID was reused;
-- treat the lock as stale and reclaim.

ALTER TABLE conversations ADD COLUMN lock_holder_started_at INTEGER;
