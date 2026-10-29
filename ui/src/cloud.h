#pragma once
// banana-ui: cloud sync, through banana-cloud (system/banana-cloud), which keeps the card's saves, states and games in
// step with a WebDAV folder shared with Slot on the TrimUI Brick and the RG SP. The account is banana/cloudsync.ini;
// without it nothing runs. The menu runs the saves and the states on its own (a while after the start, after each game,
// every 10 minutes); Settings > Network > Cloud sync runs everything, the games too, and shows how far it got.
#include <string>
#include <vector>

namespace banana
{
	namespace cloud
	{
		// banana/cloudsync.ini is there (and not enabled=0)
		bool configured(const std::string& dataDir);
		// banana-cloud in the background ("all": the games too); nothing when one is running
		void start(bool all);
		// the systems that can sync (system/banana-cloud/src/layout.rs's SYSTEMS, by their names on the server, as
		// Slot names them) and whether this handheld syncs them: systems= in banana/cloudsync-device.ini, every one
		// without it, none when it is empty. One left out is left alone, on the card and on the server.
		struct System { std::string id, name; bool picked = true; };
		std::vector<System> systems(const std::string& dataDir);
		// writes the choice back (no systems= when all are picked, so a system added later syncs too)
		bool setSystems(const std::string& dataDir, const std::vector<System>& all);
		// the games deleted from the server and still on the card, waiting for an answer: banana/cloudsync-gone.txt
		// (banana-cloud lists them there after a run of the games), less the ones already answered. Their server
		// paths: "Games/GBA/Zelda.gba"
		std::vector<std::string> gone(const std::string& dataDir);
		// a game's name for a list: "Zelda (GBA)"
		std::string goneName(const std::string& rel);
		// the answers, for the next run of the games (banana/cloudsync-answers.txt): true deletes the game on the card
		// too, false keeps it here only (it isn't sent back)
		bool answer(const std::string& dataDir, const std::vector<std::pair<std::string, bool>>& answers);
		// the running one stopped between files (a game is starting: it never writes under a game)
		void stop();
		bool running();
		// its line in /run/banana/cloud.status: "running <done> <total> <what>|<file>", "done <sent> <received>
		// <conflicts> <skipped>", "offline <why>", "error <why>", "off"; empty before the first run
		std::string status();
		// the line for a settings row: "Not set up", "Syncing", "3 / 40", "Up to date", "2 sent, 1 received",
		// "No connection", "Didn't work"
		std::string summary(const std::string& line, bool configured);
	}
}
