-- Provider and endpoint max_retries used to be auto-filled with 2 by the admin API
-- while the execution loop ignored them. The column is now an explicit same-key
-- retry override; NULL inherits the routing policy, so the auto-filled default is
-- cleared and only deliberately configured values keep overriding.
UPDATE providers SET max_retries = NULL WHERE max_retries = 2;
UPDATE provider_endpoints SET max_retries = NULL WHERE max_retries = 2;
