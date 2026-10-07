SELECT sum(sipHash64(number))
FROM numbers_mt(100000000000)
SETTINGS max_threads = 18, max_execution_time = 90,
         timeout_overflow_mode = 'break';
