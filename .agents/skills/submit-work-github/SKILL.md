---
name: submit-work-github
description: Use this to submit your work as a github PR.
---

# Submitting work with jj mega merge

## Overview

This repo uses jj with a **mega merge** workflow. Multiple independent feature branches are merged together into a single local merge commit, so you always work with all in-progress changes combined. Each feature branch becomes a separate PR.

## Key concepts

- **Mega merge**: A local-only merge commit that combines all active feature branches. The `mega-merge` bookmark.
- **Feature branches**: Independent commits between trunk and the mega merge. Each one maps to a PR.
- **Bookmarks**: Auto-generated branch names (like `ma/kptlp`) created on push.
- **Working copy**: Sits on top of the mega merge, where you make changes.

## Submitting new work

You have made some changes, now it is time to submit your work as PR.
Main decisions:
- What parts of local changes to submit? - use `jj status` to find work in current working copy.
- How should you stack the work
  1. Use jj log to get current graph
  2. If these changes should be part of existing commit?
    2.1 jj squash --into <existing-change-id> -m <conv commit> <files...>
    2.2 jj git push -r <existing-change-id>
  3. Where should these changes be based on other commits/PR or trunk
    3.1 jj squash --after <base> --before mega-merge -m <conv commit> <files...>
    3.2 jj git push -c <new-change-id> - This auto-creates a bookmark named `ma/<change-id-prefix>` and pushes it.
    3.3 oct pr create --head <bookmark> --title "PR title" --body "PR description" [--base <base-bookmark>]


## Notes

--after should be a parent of mega mega, never be a child or the mega mega itself.

```bash
oct pr create --help
  -H, --head <HEAD>    Source bookmark for the pull request
  -B, --base <BASE>    Base bookmark for the pull request - skip it for master, but use parent bookmark for stacked.
  -t, --title <TITLE>  Title for the pull request
  -b, --body <BODY>    Body for the pull request

jj git push --help
  -r, --revisions <REVSETS>
          Push bookmarks pointing to these commits (can be repeated)

  -c, --change <REVSETS>
          Push this commit by creating a bookmark (can be repeated)

          The created bookmark will be tracked automatically.
```

## Revsets aliases

| Alias | Meaning |
|-------|---------|
| `trunk` | trunk (master@origin or main@origin) |
| `mega-merge` | mega merge bookmark |
