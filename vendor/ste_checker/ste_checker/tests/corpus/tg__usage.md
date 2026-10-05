### Commands

#### Server
Start a background server that continuously syncs messages from configured forum groups:
```sh
tg server                    # default 1m pull interval
tg server --pull-interval 5m # custom interval
```

The server:
- Discovers all topics in configured forum groups at startup
- Pulls message history via MTProto
- Syncs new messages on the configured interval
- Creates markdown files in `~/.local/share/tg/<group_name>/`

Note: New topics created after server start won't appear until restart.

#### Pull
One-shot pull of messages from all configured forum groups:
```sh
tg pull
```

#### Open
Open a topic file in `$EDITOR` (uses fzf for pattern matching):
```sh
tg open           # fzf over all topics
tg open journal   # open if unique match, else fzf
```

#### Send
Send a message to a topic:
```sh
tg send journal "Today I'm feeling blue"
tg send -g 2244305221 -t 7 "direct by IDs"
```

#### List
List all discovered topics:
```sh
tg list
```

#### Todos
Aggregate TODOs from all topic files:
```sh
tg todos
```

#### Bot Info
Retrieve information about the bot:
```sh
tg bot-info
```

### Configuration
Create a configuration file at `~/.config/tg.toml`:
```toml
localhost_port = 59753
max_messages_per_chat = 1000

# MTProto credentials (required for pull/server)
# Get these from https://my.telegram.org/
api_id = 12345
api_hash = "your_api_hash"  # or use TELEGRAM_API_HASH env var
phone = "+1234567890"       # or use PHONE_NUMBER_FR env var
username = "@yourusername"  # for session file naming

[groups]
personal = "-1002244305221"        # forum group
work = "-1002244305221/3"          # specific topic in group
```

Groups are specified by chat ID (with -100 prefix for supergroups). Append `/topic_id` to target a specific topic.

Example config: examples/config.toml

#### Environment Variables
```sh
export TELEGRAM_MAIN_BOT_TOKEN="your_bot_token"  # for send/bot-info
export TELEGRAM_API_HASH="your_api_hash"         # alternative to config
export PHONE_NUMBER_FR="+1234567890"             # alternative to config
```
