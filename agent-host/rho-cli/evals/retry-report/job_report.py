"""Summarize completed CI jobs for the dashboard."""


def summarize_jobs(rows):
    counts = {"passed": 0, "failed": 0, "cancelled": 0}
    duration = 0
    for row in rows:
        status = row["status"]
        if status in counts:
            counts[status] += 1
            duration += row["duration_seconds"]
    return {
        "total_jobs": sum(counts.values()),
        "counts": counts,
        "duration_seconds": duration,
    }
