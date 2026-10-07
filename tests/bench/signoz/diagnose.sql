-- Run against the retained incident history. All timestamps use UTC.
SELECT name, value
FROM system.server_settings
WHERE name IN ('global_profiler_real_time_period_ns', 'global_profiler_cpu_time_period_ns', 'background_schedule_pool_size')
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2;

SELECT event_time_microseconds
FROM system.trace_log
WHERE event_time >= toDateTime('2026-10-07 11:19:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 11:20:00', 'UTC')
  AND trace_type = 'Real'
  AND thread_id = 309
ORDER BY event_time_microseconds
LIMIT 35
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2;

SELECT
    toStartOfMinute(event_time) AS minute,
    count() AS cpu_samples,
    countIf(startsWith(demangle(addressToSymbol(trace[1])), 'DB::QueryProfilerReal::signalHandler')) AS real_handler_samples
FROM system.trace_log
WHERE event_time >= toDateTime('2026-10-07 11:18:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 12:45:00', 'UTC')
  AND trace_type = 'CPU'
GROUP BY minute
ORDER BY minute
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2, allow_introspection_functions = 1;

SELECT
    toStartOfMinute(event_time) AS minute,
    count() AS metric_rows,
    sum(ProfileEvent_QueryProfilerRuns) AS profiler_runs,
    sum(ProfileEvent_QueryProfilerSignalOverruns) AS signal_overruns,
    sum(ProfileEvent_QueryProfilerConcurrencyOverruns) AS concurrency_overruns
FROM system.metric_log
WHERE event_time >= toDateTime('2026-10-07 11:18:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 11:45:00', 'UTC')
GROUP BY minute
ORDER BY minute
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2;

SELECT toStartOfMinute(event_time) AS minute, max(value) AS peak_load_average_1
FROM system.asynchronous_metric_log
WHERE event_time >= toDateTime('2026-10-07 11:18:00', 'UTC')
  AND event_time < toDateTime('2026-10-07 11:42:00', 'UTC')
  AND metric = 'LoadAverage1'
GROUP BY minute
ORDER BY minute
FORMAT JSONEachRow
SETTINGS max_execution_time = 10, max_threads = 2;
