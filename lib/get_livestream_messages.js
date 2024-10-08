import { config } from "../configs/load_user_config.js";
import { CallTTS } from "./call_tts.js";
import { IntTimer } from './IntTimer.js';

let stream_data = {
	"videoId": undefined,
	"liveChatId": undefined,
	"apiKey": undefined,
}

let new_messages = [{}]; // according to youtubes v3 api, this is how the new_messages will be received
export let messages = [{}];
//new messages is the return from the api. messages is messages that have been filtered

export let message_index = 0;

let tts_queue = 0;


let FetchTimer;
export async function StartGettingMessages(args = { oneshot: true }) {
	args.timeoutDuration = document.getElementById("timer_freq").value
	FetchTimer = new IntTimer(
		{
			timerName: "fetchMessagesTimer",
			timeoutDuration: args.timeoutDuration,
			killOnTimeout: args.oneshot,
		}
	);

	let looper = (data) => {
		console.log("timeout found, fetching new_messages");

		ConfigApiCalls();
		//ReadNextMessage(); BUG: ERROR HERE
	}

	// init call to be responsive
	ConfigApiCalls();
	// ADD LIMITER SO ONLY ONE CAN HAPPEN AT A TIME
	FetchTimer.Connect(looper);
}

export async function StopGettingMessages() { //called in index.html
	FetchTimer.Kill();
}



// all this does is get the liveChat id
async function ConfigApiCalls() {
	stream_data.videoId = config.user_config.stream_url;
	stream_data.apiKey = config.global_config.APIs.gcloud_key;

	console.log("stream_url:", stream_data.videoId);
	console.log("api key: ", stream_data.apiKey);


	if (stream_data.liveChatId == undefined || stream_data.apiKey == undefined) { // don't get a livechat id every call cause that'll double api consumption
		try {
			const response = await fetch(`https://www.googleapis.com/youtube/v3/videos?part=liveStreamingDetails&id=${stream_data.videoId}&key=${stream_data.apiKey}`);
			const data = await response.json()
			stream_data.liveChatId = data.items[0].liveStreamingDetails.activeLiveChatId;

			console.log("Live Chat ID:", stream_data.liveChatId, "\n", "api:", stream_data.apiKey);

			await GetAndUpdateMessages({ "apiKey": stream_data.apiKey, "liveChatId": stream_data.liveChatId, });
		}
		catch (err) {
			console.error(err);
			FetchTimer.Kill(); // no need to run further
		}
	}
	else {
		await GetAndUpdateMessages({ "apiKey": stream_data.apiKey, "liveChatId": stream_data.liveChatId, });
	}

	return;
}

async function GetAndUpdateMessages(args = { "apiKey": undefined, "liveChatId": undefined, }) {
	console.log("UpdateMessages()");
	console.log("api:", args.apiKey, "\n", "Live Chat ID:", args.liveChatId, "\n");

	//quick check
	if (args.liveChatId == undefined || args.apiKey == undefined) {
		console.warn("unable to get messages due to liveChatId or apiKey being undefined:", args.liveChatId, args.apiKey);
		FetchTimer.Kill();
		return;
	}

	await fetch(`https://www.googleapis.com/youtube/v3/liveChat/messages?liveChatId=${args.liveChatId}&part=snippet,authorDetails&key=${args.apiKey}`)
		.then(response => response.json())
		.then(data => {
			console.log("DATA FROM FETCH: ", data);
			new_messages = data.items.map(item => ({
				author: item.authorDetails.displayName,
				message: item.snippet.displayMessage,
				time: item.snippet.publishedAt
			}));
		})
		.catch(err => console.error(err));

	//return (new_messages);
	console.log("messages", messages, "\n", "new_messages", new_messages);

	await FilterOldMessages();

}

async function FilterOldMessages() {
	console.log("FilterOldMessages()");

	messages = new_messages
	// //filter old from new messages
	// let is_existing;
	// let new_messages_count = 0;

	// messages = undefined;

	// if (messages == undefined) { //if undefined, just add all messages
	// 	console.log("messages undefined, adding all new messages");
	// 	messages = new_messages
	// 	new_messages_count = new_messages.length;
	// }
	// else {
	// 	for (let i = 0; i < new_messages.length; ++i) {
	// 		is_existing = true;
	// 		for (let j = 0; j < messages.length; ++j) {
	// 			if (new_messages[i] != messages[j]) {
	// 				is_existing = false;
	// 				break;
	// 			}
	// 			continue;
	// 		}
	// 		if (is_existing == true) {
	// 			console.log("old message found, skipping", new_messages[i]);
	// 		}
	// 		else {
	// 			console.log("new message found, adding:", new_messages[i]);
	// 			messages.push(new_messages[i]);
	// 		}
	// 		continue;
	// 	}
	// }

	// console.log("ADDING NEW MESSAGES:", new_messages, '\n', "total new messages:", new_messages_count);

	await AddMessagesToMessageList(new_messages);
	return;
}



async function AddMessagesToMessageList() {
	console.log('AddMessagesToMessageList()');
	console.log("NEW MESSAGES:", new_messages, '\n', "messages:", messages);

	try {
		let newElem, playElem, textElem, skipElem;

		console.log("child_count = ", document.getElementById("messages_list").childElementCount);
		document.getElementById("messages_list").innerHTML = "";

		for (let i = 0; i < messages.length; i++) {

			//skip existing
			if (document.getElementById(`ml_li${i}`)) {
				break;
			}

			let ml = document.getElementById("messages_list");

			newElem = document.createElement("li");
			newElem.setAttribute("isTTS", "false");
			newElem.id = `ml_li${i}`;
			newElem.class = "ml_item";

			playElem = document.createElement("button");
			playElem.innerText = "▶️";
			playElem.addEventListener("mousedown", (e) => { // BUG: THIS TTS_QUEUE ISN'T ACCURATELY UPDATING
				CallTTS(document.getElementById(`message${i}`).innerText);

				if (
					document.getElementById(`ml_li${i}`).getAttribute("isTTS") == "true" &&
					document.getElementById(`ml_li${i}`).getAttribute("TTSPlayed") == "false"
				) {
					if (document.getElementById(`ml_li${i}`).style.backgroundColor != "#222") {
						document.getElementById(`ml_li${i}`).style.backgroundColor = "#222";
						if (tts_queue > 0) {
							tts_queue -= 1;
						}
					}
				}

			});
			newElem.appendChild(playElem);

			skipElem = document.createElement("button");
			skipElem.innerText = "❌";
			skipElem.addEventListener("mousedown", (e) => {
				let skipTTS = document.getElementById(`ml_li${i}`).getAttribute("isTTS");

				if (skipTTS == false) {
					document.getElementById(`ml_li${i}`).setAttribute("skiptts", "true");
					document.getElementById(`ml_li${i}`).style.backgroundColor = "#000";
					document.getElementById(`ml_li${i}`).style.color = "#666";
				}
				else {
					document.getElementById(`ml_li${i}`).setAttribute("skiptts", "false");
					document.getElementById(`ml_li${i}`).style.backgroundColor = "#028";
					document.getElementById(`ml_li${i}`).style.color = "#fff";
				}
			});
			newElem.appendChild(skipElem);

			textElem = document.createElement("span");
			textElem.id = `message${i}`
			newElem.appendChild(textElem);

			if (new_messages[i].message.slice(0, 4) == "!tts") {
				newElem.setAttribute("isTTS", "true");
				newElem.setAttribute("TTSPlayed", "false");
				newElem.setAttribute("skipTTS", "false");

				textElem.innerText = `user: ${new_messages[i].author} says; ${new_messages[i].message.slice(4, new_messages[i].message.length)}`;
			}
			else {
				textElem.innerText = `user: ${new_messages[i].author} says; ${new_messages[i].message}`;
			}



			if (new_messages[i].message.includes("!tts")) {
				newElem.style.backgroundColor = "#028";
				tts_queue += 1;
			}
			else {
				newElem.style.backgroundColor = '#200';
			}



			ml.appendChild(newElem);
		}
	}
	catch (err) {
		console.log(err);
	}

	document.getElementById("tts_queue_indicator").innerText = tts_queue;
}
