# Read Google Drive and Meet from a rho host

The host's `google.sock` proxy reads Google Drive v3 and Meet v2 conference records
as the Workspace user you authorize. The host keeps the OAuth credential; agents
make ordinary GET requests over the Unix socket without a token or a Google login.
In this guide, replace `agent@example.org` with the account you want rho to use.

## Set up the operator credential

1. In a Google Cloud project **inside that account's Workspace organization**,
   [enable](https://developers.google.com/workspace/guides/enable-apis) the
   Google Drive API (`drive.googleapis.com`) and Google Meet REST API (`meet.googleapis.com`).
2. In **Google Auth Platform → Branding**, complete the
   [consent screen setup](https://developers.google.com/workspace/guides/configure-oauth-consent)
   and set **Audience** to **Internal**. If Internal isn't available, check the
   selected project's organization. Under **Clients → Create client**, choose
   [**Desktop app**](https://developers.google.com/workspace/guides/create-credentials#desktop-app)
   and download its JSON. Use a dedicated client for rho. If Workspace app access
   controls block consent, ask the Workspace administrator to allow that client
   for Drive and Meet.
3. On a **trusted workstation with a browser**, not on the agent host or in an
   agent shell or workset, obtain an offline grant for just these two scopes:
   `https://www.googleapis.com/auth/drive.readonly` and
   `https://www.googleapis.com/auth/meetings.space.readonly`. Use Google's
   [installed-app OAuth library](https://googleapis.dev/python/google-auth-oauthlib/latest/reference/google_auth_oauthlib.flow.html)
   with a [loopback redirect](https://developers.google.com/identity/protocols/oauth2/native-app).
   Confirm `agent@example.org` and the requested access in the consent screen.

On the workstation, start a private temporary directory and save the downloaded
Desktop client JSON as `client.json` inside it:

```sh
umask 077
work=$(mktemp -d)
printf 'Save the Desktop client JSON to %s/client.json\n' "$work"
```

Run the following in the same shell:

```sh
python3 -m venv "$work/venv" &&
  "$work/venv/bin/python" -m pip install google-auth-oauthlib

"$work/venv/bin/python" - "$work/client.json" "$work/authorized-user.json" <<'PY'
import json
import sys
from pathlib import Path

from google.auth.transport.requests import AuthorizedSession
from google_auth_oauthlib.flow import InstalledAppFlow

account = "agent@example.org"
flow = InstalledAppFlow.from_client_secrets_file(
    sys.argv[1],
    scopes=[
        "https://www.googleapis.com/auth/drive.readonly",
        "https://www.googleapis.com/auth/meetings.space.readonly",
    ],
    autogenerate_code_verifier=True,
)
grant = flow.run_local_server(
    host="127.0.0.1", port=0, access_type="offline",
    login_hint=account, prompt="consent",
    authorization_prompt_message=None, timeout_seconds=300,
)
if not grant.refresh_token:
    raise SystemExit("Google did not issue a refresh token")

with AuthorizedSession(grant) as session:
    about = session.get(
        "https://www.googleapis.com/drive/v3/about",
        params={"fields": "user(emailAddress)"}, timeout=30,
    )
    about.raise_for_status()
    if about.json().get("user", {}).get("emailAddress") != account:
        raise SystemExit(f"Authorize {account} before importing a grant")

with Path(sys.argv[2]).open("x", encoding="utf-8") as output:
    json.dump({
        "type": "authorized_user",
        "client_id": grant.client_id,
        "client_secret": grant.client_secret,
        "refresh_token": grant.refresh_token,
    }, output)
PY
```

## Install on the host

Start the intended rho host service if it isn't already running. From that
same trusted workstation, stream the JSON into `rho google init` over SSH as
the user running the host service. This also works for a headless host: the
browser and credential files stay on the workstation. Replace `your-agent-host`
with the SSH destination. For a non-default host runtime, use
`rho google --runtime-dir /absolute/path init` in the SSH command.

```sh
ssh -T your-agent-host 'rho google init' < "$work/authorized-user.json"
```

The import validates and installs only the four `authorized_user` fields in the
host's RAM-only platform secret store; it does not attempt a Google login.
Re-import to replace the grant. The success message indicates whether the
secret was stashed in the systemd FD store for service restarts. If the store
is lost, obtain and import a grant again.

Do not paste credentials into the terminal, put them in command arguments or
environment variables, or copy either JSON file into the host's home or a
workset. Rho agents aren't isolated from files readable by the host user.
After confirming the requests below work, remove the temporary directory
(`rm -rf -- "$work"`) and any extra downloaded copy. Deleting the local files
doesn't revoke the installed grant; revoke the app's access in the Google account
if you need to invalidate it.

## Read over the socket

Run these commands on the agent host. Use the host's `RHO_RUNTIME_DIR` (normally
`$XDG_RUNTIME_DIR/rho` if it isn't already set). `http://localhost` is just the
HTTP URL for curl; the connection goes to the Unix socket.

### Drive

Search visible, non-trashed files. Use a result's `id` for metadata, binary
content with `alt=media`, or export for a Google Docs file:

```sh
sock="${RHO_RUNTIME_DIR:-$XDG_RUNTIME_DIR/rho}/google.sock"
curl -fsS --unix-socket "$sock" --get http://localhost/drive/v3/files \
  --data-urlencode 'q=trashed = false' \
  --data-urlencode 'fields=nextPageToken,files(id,name,mimeType,webViewLink)'

file_id=REPLACE_WITH_FILE_ID
curl -fsS --unix-socket "$sock" "http://localhost/drive/v3/files/$file_id"
curl -fsS --unix-socket "$sock" "http://localhost/drive/v3/files/$file_id/export?mimeType=text%2Fplain"
```

Share the intended files or folders with `agent@example.org` as Viewer, or grant
it access in the relevant shared drive. The proxy can read whatever this account
can read, not just files shared directly with it. For shared-drive search use
[Drive's shared-drive parameters](https://developers.google.com/workspace/drive/api/guides/enable-shareddrives)
as needed, such as `supportsAllDrives`, `includeItemsFromAllDrives`, `corpora=drive`
and `driveId`.

### Meet

Use the returned `name` values, not the meeting code. The conference name is
`conferenceRecords/…`; the transcript name includes the conference prefix.

```sh
sock="${RHO_RUNTIME_DIR:-$XDG_RUNTIME_DIR/rho}/google.sock"
curl -fsS --unix-socket "$sock" http://localhost/v2/conferenceRecords

conference=conferenceRecords/REPLACE_WITH_CONFERENCE_ID
curl -fsS --unix-socket "$sock" "http://localhost/v2/$conference/transcripts"

transcript="$conference/transcripts/REPLACE_WITH_TRANSCRIPT_ID"
curl -fsS --unix-socket "$sock" "http://localhost/v2/$transcript/entries"
```

For either API, send `nextPageToken` back as `pageToken` on the next request
with the same filters (use `--get --data-urlencode "pageToken=$token"`).

Meet doesn't give this account organization-wide access. Google controls
access to meeting records and artifacts; for example, Google documents
[artifact reads](https://developers.google.com/workspace/meet/api/guides/artifacts)
for meeting space owners or participants. Transcripts exist only when transcription
was enabled for the conference and may take time to generate. The Meet
[conference record](https://developers.google.com/workspace/meet/api/reference/rest/v2/conferenceRecords)
and transcript entries expire 30 days after the conference ends. The generated
Docs transcript lives in the organizer's Drive under its separate sharing and
retention rules. If the Meet record isn't accessible or has expired, ask the
organizer to share the transcript Doc with `agent@example.org` and use Drive export.
When available, `docsDestination.document` in the Meet transcript response
is the Drive file ID.
