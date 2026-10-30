-- Read retained incident history with clickhouse-client --multiquery.
-- Trace timestamps identify collector reads, not signal-handler capture times.


SELECT event_time_microseconds, thread_id,
       arrayMap(x -> demangle(addressToSymbol(x)), trace) AS frames
FROM system.trace_log
WHERE event_time >= toDateTime('2026-10-07 11:29:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 11:35:00', 'UTC')
  AND trace_type = 'Real' AND thread_id = 496
ORDER BY event_time_microseconds
LIMIT 1000
FORMAT JSONEachRow
SETTINGS allow_introspection_functions = 1, max_execution_time = 10, max_threads = 2;

SELECT event_time, count() AS real_samples, uniqExact(thread_id) AS threads
FROM system.trace_log
WHERE event_time >= toDateTime('2026-10-07 11:18:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 11:40:00', 'UTC')
  AND trace_type = 'Real'
GROUP BY event_time
ORDER BY event_time
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2;

SELECT event_time, ProfileEvent_OSCPUWaitMicroseconds, ProfileEvent_OSIOWaitMicroseconds,
       ProfileEvent_SoftPageFaults, ProfileEvent_HardPageFaults,
       ProfileEvent_UserTimeMicroseconds, ProfileEvent_SystemTimeMicroseconds,
       ProfileEvent_QueryProfilerRuns, ProfileEvent_QueryProfilerSignalOverruns,
       ProfileEvent_QueryProfilerConcurrencyOverruns
FROM system.metric_log
WHERE event_time >= toDateTime('2026-10-07 11:29:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 11:35:00', 'UTC')
ORDER BY event_time
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2;

SELECT event_time, count() AS metric_count,
       maxIf(value, metric = 'LoadAverage1') AS load1,
       maxIf(value, metric = 'OSCPUOverload') AS overload,
       maxIf(value, metric = 'OSThreadsRunnable') AS runnable,
       sumIf(value, startsWith(metric, 'OSIOWaitTimeCPU')) AS iowait,
       sumIf(value, startsWith(metric, 'OSStealTimeCPU')) AS steal
FROM system.asynchronous_metric_log
WHERE event_time >= toDateTime('2026-10-07 11:29:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 11:35:00', 'UTC')
  AND (metric IN ('LoadAverage1', 'OSCPUOverload', 'OSThreadsRunnable')
       OR startsWith(metric, 'OSIOWaitTimeCPU') OR startsWith(metric, 'OSStealTimeCPU'))
GROUP BY event_time
ORDER BY event_time
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2;
