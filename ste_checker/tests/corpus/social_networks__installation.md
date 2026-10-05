```sh
cargo install --git https://github.com/valeratrades/social_networks --branch master
```

## Email Setup

The email command supports two authentication methods:

### Option 1: IMAP with App Password (simpler)
1. Enable 2-Step Verification on your Google account
2. Go to [Google App Passwords](https://myaccount.google.com/apppasswords)
3. Generate an app password for "Mail"
4. Add to your config:
   ```toml
   [email]
   email = "your@gmail.com"
   [email.auth.imap]
   pass = "your-app-password"
   ```

### Option 2: OAuth (for remote servers)
1. Create a project in [Google Cloud Console](https://console.cloud.google.com/)
2. Enable the Gmail API
3. Create OAuth 2.0 credentials (Desktop app)
4. Add to your config:
   ```toml
   [email]
   email = "your@gmail.com"
   [email.auth.oauth]
   client_id = "..."
   client_secret = "..."
   ```
5. Run `social_networks email` on your **local machine** (where you can open a browser)
6. Complete the OAuth authentication flow
7. Copy the token file to the remote server:
   ```sh
   scp ~/.local/state/social_networks/gmail_tokens.json <remote-server>:~/.local/state/social_networks/
   ```
