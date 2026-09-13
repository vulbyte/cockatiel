# Cockatiel


## roadmap for v1:

#### checkpoint 1: da core
- [x] - Protocol Stability First: Prioritize locking down the Protobuf schema over adding new features.
- [ ] - one file import for the following langs:
    - [ ] - C (#import <cockatiel_lib.h/c> or cmake)
    - [ ] - C# (dotnet package?)
    - [ ] - gdScript (file/folder you paste into your project, then const MyLib = preload("res://path/to/external_lib.gd"))
    - [ ] - javaScript (ie: import { chunk } from './libs/cockatiel_lib.js';)
        - [ ] - ez import
        - [ ] - README.md tutorial
        - [ ] - <import_name>.connect()
        - [ ] - <import_name>.Send()
        - [ ] - <import_name>.ReceiveAny()
        - [ ] - <import_name>.Receive.protobuffType()
    - [ ] - python (cockatiel = get("https://github.com/vulbyte/cockatiel_lib"))
        - [ ] - ez import
        - [ ] - README.md tutorial
        - [ ] - <import_name>.connect()
        - [ ] - <import_name>.Send()
        - [ ] - <import_name>.ReceiveAny()
        - [ ] - <import_name>.Receive.protobuffType()
    - [x] - rust (use "path/to/file/cockatiel_lib.rs")
        - [x] - ez import
        - [x] - README.md tutorial
        - [x] - <import_name>.connect()
        - [x] - <import_name>.Send(<protobuffType>, <tareget_as_string>, <message>)
        - [x] - <import_name>.Receive.protobuffType(<functionToCatchData>)
- [ ] - test input module, which will connect as: adapter, preprocess, inprocess, and postprocess, to verify dataflow
    - [ ] - message Container { /* general template for communicating messages */
        - [ ] -   int32 version = 1; /* version for disbatching */
        - [ ] -   string auth_token = 3; /* JWT for auth */
        - [ ] -   string module_name = 4; /* used for displaying to user and for internal resource mgmt */
        - [ ] -   string module_instance_uuid7 = 5; /*if is a connection request, will ignore this*/
            - [ ] -   oneof payload {
                - [ ] -     ConnectionRequest connection_request = 7;
                - [ ] -     ConnectionRequestReturn connection_request_return = 8;
                - [ ] -     AuthVerify auth_verify = 9;
                - [ ] -     AuthNew auth_new = 10;
                - [ ] -     Command command_payload = 11;
                - [ ] -     Commands commands_payload = 12;
                - [ ] -     MessagePreProcess message_pre_process = 13;
                - [ ] -     MessageInProcess message_in_process = 14;
                - [ ] -     MessagePostProcess message_post_process = 15;
                - [ ] -     TimelineEvent timeline_event = 16;
                - [ ] -     UserData user_data = 17;
                - [ ] -     Shutdown shutdown = 18;
                - [ ] -     Log log = 19;
                - [ ] -     Err err = 20;
                - [ ] -     SendToPlatfroms send_to_platforms = 21;  /*if received and module has perms, used to tell the engine to send a message to all chats*/
            - [ ] -   }

#### checkpoint 2: engine modules
- [ ] - timeline_database
    - [ ] - check and verify config
    - [ ] - create config
    - [ ] - add event
    - [ ] - get event
- [ ] - user_database
    - [ ] - check and verify config
    - [ ] - create config
    - [ ] - check if config exists and is valid
    - [ ] - init table
    - [ ] - user control
        - [ ] - read user
        - [ ] - write user
        - [ ] - delete user
    - [ ] - user value control
        - [ ] - read value from user (via uuid7)
        - [ ] - write value from user (via uuid7)
        - [ ] - delete user from user (via uuid7)
    - [ ] - subprocess that connects and listens for the following commands:
        - [ ] - commendment 
        - [ ] - reprimand
        - [ ] - ban
        - [ ] - timeout


#### checkpoint 3: platform modules
- [ ] - discord_adapter 
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - impl cockatiel_lib for protobuf communication
   - [ ] - receive messages from as specific channel in specific server
   - [ ] - receive messages from an entire server
   - [ ] - pass messages as a preprocessed module to the engine
   - [ ] - receive a "Send" protobuf send messages into discord channel as a "user" (aka: cockatiel: message)
   - [ ] - receive a "Send" send messages into discord channel as an "embed" (https://discordjs.guide/legacy/popular-topics/embeds)
   - [ ] - ban user
       - [ ] - -d --duration
       - [ ] - -r --reason
   - [ ] - timeout user
       - [ ] - -d --duration
       - [ ] - -r --reason
-[ ] - kick_adapter
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - impl cockatiel_lib for protobuf communication
   - [ ] - read messages from chat 
   - [ ] - pass messages as a preprocessed module to the engine
   - [ ] - send messages into chat
   - [ ] - ban user
       - [ ] - -d --duration
       - [ ] - -r --reason
   - [ ] - timeout user
       - [ ] - -d --duration
       - [ ] - -r --reason
- [ ] - twitch_adapter
    - [ ] - check and verify config
   - [ ] - create config
   - [ ] - impl cockatiel_lib for protobuf communication
   - [ ] - read messages from chat 
   - [ ] - pass messages as a preprocessed module to the engine
   - [ ] - send messages into chat
   - [ ] - ban user
       - [ ] - -d --duration
       - [ ] - -r --reason
   - [ ] - timeout user
       - [ ] - -d --duration
       - [ ] - -r --reason
- [ ] - youtube_adapter
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - impl cockatiel_lib for protobuf communication
   - [ ] - read messages from chat 
   - [ ] - pass messages as a preprocessed module to the engine
   - [ ] - send messages into chat
   - [ ] - ban user
       - [ ] - -d --duration
       - [ ] - -r --reason
   - [ ] - timeout user
       - [ ] - -d --duration
       - [ ] - -r --reason

#### checkpoint 4: processing modules
- [ ] - banned words manager
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - impl cockatiel_lib for protobuf communication
   - [ ] - parse message for banned words
       - [ ] - no spaces (thisisastringofbadwords)
       - [ ] - leet words (ch33z3)
       - [ ] - extra spaces (b a n n e d _ w o r d)
   - [ ] - flag message for review
   - if banned word is found:
   - [ ] - soft censor word (a food i really don't like is c!!!!e)
   - [ ] - mid censor word (a food i really don't like is c!!!!!)
   - [ ] - hard censor word (a food i really don't like is !!!!!!)
   - [ ] - replace word (a food i really don't like is [cheese -> apples])
   - [ ] - replace replace sentance ([a food i really don't like is cheese -> sometimes i dream of cheese])
   - [ ] - censor sentance (!!!!!!!)
   - [ ] - OPTIONAL: lightweight llm review such as 
       - [ ] - thurough checking with flags [llama-guard-3-1b](https://huggingface.co/meta-llama/Llama-Guard-3-1B) 
       - [ ] - quick check with 0-1 probability [deberta-v3-small](https://huggingface.co/microsoft/deberta-v3-small)
- [ ] - score messages (good reference is the sustem for animal crossing new horizons)
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - contextual rewards/punishments
       - [ ] - punctuation
           - [ ] - toggle
           - [ ] - user setable score value
       - [ ] - questions
           - [ ] - toggle
           - [ ] - user setable score value
       - [ ] - length
           - [ ] - toggle
           - [ ] - user setable score value
       - [ ] - emoji
       - [ ] - frequency (too fast/slow)
           - [ ] - toggle
           - [ ] - user setable score value
       - [ ] - bad punctuation
           - [ ] - toggle
           - [ ] - user setable score value
       - [ ] - spam (ie: asdlfkjsqaldkfja, bbbbbb)
           - [ ] - toggle
           - [ ] - user setable score value
       - [ ] - no spacing 
           - [ ] - toggle
           - [ ] - user setable score value
       - [ ] - wordless messages (ie: use trigrams to determine if word is valid)
           - [ ] - toggle
           - [ ] - user setable score value
- [ ] - tts_module
   - [ ] - impl cockatiel_lib for protobuf communication
   - [ ] - allow users to choose custom model form local file
   - [ ] - take in string via PostProcessMessage
   - [ ] - render message via desired model/method
       - [ ] - add fallback if there's an error ie model missing, render fail, etc
   - [ ] - play rendered tts message
   - [ ] - volume for the tts module

#### checkpoint 5: minimal-UI
- [ ] - term_chat
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - disable custom colors (global)
   - [ ] - show user (with color)
       - [ ] - toggle for username 
       - [ ] - toggle for color 
   - [ ] - show platform
       - [ ] - toggle for username 
       - [ ] - toggle for color 
   - [ ] - show user status  (ie sponsor/mod/admin/owner and commendation/reprimand, either being colored)
       - [ ] - toggle for user priv
       - [ ] - toggle for user repriment
       - [ ] - toggle for colors
   - [ ] - show user rank (ie opal/trash/other)
       - [ ] - toggle for display
       - [ ] - toggle for colors
   - [ ] - disappear after x seconds
       - [ ] - options for how the message is removed
       - [ ] - toggle on/off
       - [ ] - toggle for setting
   - [ ] - message easing to let the chat messages feel "smoother" (display messages slower if there's a burst (but not too much that they overflow and some never get seen), and send instantly if there hasn't been messages in a while)
       - [ ] - target messages/minute
       - [ ] - toggle on/off (if off send instantly)
   - [ ] - emoji map, ie take in :emoji_from_platform:
       - [ ] - toggle for setting
   - [ ] - display images and gifs as aciiart
       - [ ] - toggle
       - [ ] - adaptive to size
       - [ ] - use [TheZoraiz/ascii-image-conver](https://github.com/TheZoraiz/ascii-image-converter) maybe? (not an endorcement, need to inspect)

#### checkpoint 6: mod tools
- [ ] - mod_chat (fork that builds ontop of term_chat)
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - show user status  (ie sponsor/mod/admin/owner and commendation/reprimand)
       - [ ] - toggle for setting
   - [ ] - show user rank (ie opal/trash/other)
       - [ ] - toggle for setting
   - [ ] - disappear after x seconds
       - [ ] - toggle for setting
   - [ ] - disable custom colors
       - [ ] - toggle for setting
   - [ ] - message easing to let the chat messages feel "smoother" (display messages slower if there's a burst (but not too much that they overflow and some never get seen), and send instantly if there hasn't been messages in a while)
       - [ ] - toggle for setting (if off always push asap)
       - [ ] - slider for "smooth amount" (ie, target x messages per second)
   - [ ] - send message (as user/as system)
   - [ ] - display images and gifs
       - [ ] - toggle for profile pictures (display: none, after review, all)
       - [ ] - toggle for images/gifs within messages (display: none, after review, all)
   - [ ] - emoji map, ie take in :emoji_from_platform:
- [ ] - timeline_web_ui
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - get messages from user
   - [ ] - user notes
       - [ ] - get
       - [ ] - display
       - [ ] - adjsut (update on type)
   - [ ] - get errors
   - [ ] - get logs
   - [ ] - get user(s) via property (ie: bans, commendment, reprimends, totalscore, etc)
   - [ ] - display for the users properties

#### checkpoint 7: extend modules and features
- [ ] - one file import for the following langs:
   - [ ] - C++ (#import <cockatiel_lib.h/c> or cmake)
   - [ ] - odin (import "<path/to/file/cockatiel_lib>")
   - [ ] - java (???)
   - [ ] - lua (local my_lib = require("mymodule"))
- [ ] - language constrainer 
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - impl cockatiel_lib for protobuf communication
   - [ ] - UTF-8 codes of valid characters for language
       - [ ] - toggle for emojis
       - [ ] - toggle for characters commonly used for expressive chars such as "ඞ" or "(๑ > ᴗ < ๑)" etc
   - [ ] - create map file for language and construct to said map file (plus toggles)
- [ ] - web_chat_renderer
   - [ ] - check and verify config
   - [ ] - create config
   - [ ] - take in PostProcessMessage with user data
   - [ ] - display message in css based chat window
   - [ ] - options: 
       - [ ] - disappear after x seconds
       - [ ] - animations on/off
       - [ ] - disable custom colors

 
## about the project:
tts and chat management for streamers

> why make this?
i don't find there's a good open standard for chat interactions, and the ones that do offer this sorta functionality are an absolute pain to use or expand, and instead of making a service where the entire mentality is "oh, well i'll just keep stitching these disconnected tools together", i wanted to make something more cohesive.

i also find that most of the alternatives favor a specific platform far too much, so i want a truely agnostic platform so anyone on any platform can stream and **give the viewers the best experience possible**.

i have also found that other programs are just really hard and annoying to work with and aren't very extensible. this project instead uses a queue's and signals based system which is [from my experience] far easier to extend and customize.

to keep it simple, here's the general loop:
- the program inits and verifys the config to make sure there are no issues,
- after that the main loop for the program is:
> main loop
- check/listen for an update
- add data to an unprocessed queue (this is to capture the data as fast as possible and allow for differed processing in case of performance issues or simply wanting to differ processing until a later point)
- process a message based on the platform and the data captured into the platform [cockatiel's] specific format,
- add messages to the messages_queue on success
> if there is an error at any point the error will be added to the errored_queue and can be view'd later.

while the main loop is running, there is seperate loops for each process happening aswell, 

### FAQ:
> why are you locking global syncing behind a paid cost?
because servers cost money, and i'm barley able to sustain myself. if i had affluent amounts of welth maybe, but unless you want another platform like [discord and their palantir scandal](https://www.bbc.com/news/articles/cn4g8ynpwl8o), it's best to make sure the platform is sustainable but decoupled to ensure users rights aren't violated.

> why is there no blacklist for bad works by default?
1. i have no faith my account wont get instabanned for just having a bunch of bad words and slurs due to ai moderation,
2. i'm not trying to tell you how to police your community or how it should be ran/managed. i'm not your mom and i don't know what your standards are, so i'll leave it upto you and your community to form and build how it wants.

> why are the tests private?
AI companies and what not training off the data, tests have been semi-consistently a major key point for AI companies, and although i'm not completely against AI [see my ethics on AI here](https://vulbyte.com/policies/AI), as outlined in the link, AI companies right now are what i consider to be largely immoral right now, and for that i refuse to support them as is.

> why rust for the backend?
Although i [vulbyte] prefer c, cpp, or even odin over rust, due to rust being an application being focused on frontend interactions from multiple sources, want to use the maximal balance of saftey and performance.

> why use a gplv2 license?
because i don't want this project to become some thing obfuscated/highjacked by a larger company and although companies might be able to rip off this, i want this project to be something owned by the community and something everyone can benefit from, not just me.
[cough](https://www.reddit.com/r/OutOfTheLoop/comments/qw5e2u/comment/hl0wsv6/?utm_source=share&utm_medium=web3x&utm_name=web3xcss&utm_term=1&utm_content=share_button),
[cough](https://x.com/OBSProject/status/1460782968633499651),
i

---

#### side research for possible future modules:
billi-billi: `scaping?` [scraping option](https://github.com/mistgc/bili-live-chat),
facebook: curl [docs](https://developers.facebook.com/docs/live-video-api/),
instagram: no idea [i think this is not it but worth a shot](https://developers.facebook.com/docs/messenger-platform/instagram/),
kick: curl [docs](https://docs.kick.com/apis/livestreams),
picarto: curl-http [docs](https://api.picarto.tv/),
tiktok: `scraping?` [scraping option for now](https://github.com/jpw142/rscraperTikTokLive),
twitch: curl-http [docs](https://dev.twitch.tv/docs/chat/send-receive-messages/),
twitter: curl-http/webhook [docs i think](https://docs.x.com/x-api/introduction?search=livestream+messages),
vimeo: no idea [i beleive these are the docs](https://help.vimeo.com/hc/en-us/articles/12427783601937-How-to-use-the-Vimeo-Live-API),

---

new readme.md (need to reformat, so this is a work in progress)

# Cockatiel - chat automation engine

## bluhbluhbluh

### creating a module

to connect with the engine all you need to do is attempt to connect to the engine with a given ip/port/pin, HOWEVER if you want the user to be able to start/stop/whatever else with the module, you'll need to create a `cockatiel_module_info.json` within your module to tell cockatiel how to interact with it. here's an example, and you will have to infer how to create your module from it. 
> NOTE: if your module is being ran remotely (ie on a different computer), or is managed by a different application (ie for in game communication) this will be ignored and is not needed. however if a user imports it into their ./modules/<your_module> folder, then this is crutial or else your module will be "invisable" to cockatiel.
```json
{
    // This is the name presented to Cockatiel.
    "name": "example-module", // REQUIRED, no spaces, name is concerted to all lowercase for standardization 

    // Human-readable description of what this module does.
    "description": "An example Cockatiel module.", // optional

    // Version of this module.
    "version": "0.1.0", // optional, just a string, no formatting is enforeced

    // What this module is responsivle for
    "capabilities" = (if you're unfamiliar with json, the value should be something like: `"capabilities": "postprocess",` )
        "input"         // for modules that provide chats that need to be processed. cockatiel_expects: Authmessages,Log,Err,
        "preprocess"    // mainly for archival modules
        "inprocess"     // for modules that wish to modify the chat in flight, ie: censoring, swapping words, telling cockatiel to drop the mesasge due to a flag etc
        "postprocess"   // THIS IS PROBABLY WHAT YOU WANT, used for after the message has been processed, ie for chatbots, sending message to a chat display application, passing a processed command to a game engine, etc

    "root_file" = "./main.rs" //whatever file the launcher needs to run 
    "launch_command" = "cargo", // make this whatever your program needs, such as: node, python3, cargo, etc. IF IS A COMPILED PROGRAM, THEN THE VALUE MUST BE ""
    "command_flags" = [
        "--flag value",
        "--flag value",
        "--flag value"
    ];

    // Whether Cockatiel should consider this module safe to automatically launch when configured.
    "autostart": true | false
}
```
