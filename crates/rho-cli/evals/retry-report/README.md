# CI job dashboard totals

`job_report.summarize_jobs(rows)` accepts an iterable of job-attempt records
and returns the dashboard's job counts and total duration. It currently
inflates totals when a job is retried.

Count one completed attempt per `job_id`: the highest integer `attempt`, with
the last input record winning ties. Only `passed`, `failed`, and `cancelled`
are completed statuses. Other statuses must not hide an earlier completed
attempt. A missing `duration_seconds` means zero. Preserve the return schema
and do not mutate the input records. Records may arrive out of attempt order.
