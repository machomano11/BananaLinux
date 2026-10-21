#include "cloud.h"
#include <cctype>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <signal.h>
#include <sstream>
#include <sys/wait.h>
#include <unistd.h>

namespace banana
{
	namespace cloud
	{
		// /run/banana, or BANANA_RUN's folder (banana-cloud reads it too: the headless test)
		static std::string runDir()
		{
			const char* r = getenv("BANANA_RUN");
			return r && *r ? r : "/run/banana";
		}
		static std::string statusFile() { return runDir() + "/cloud.status"; }
		static std::string lockFile() { return runDir() + "/cloud.lock"; }	// the running one's pid (empty when it ended)

		static std::string firstLine(const std::string& path)
		{
			std::ifstream f(path);
			std::string s;
			std::getline(f, s);
			while (!s.empty() && (s.back() == '\r' || s.back() == ' ')) s.pop_back();
			return s;
		}

		bool configured(const std::string& dataDir)
		{
			std::ifstream f(dataDir + "/cloudsync.ini");
			if (!f) return false;
			bool url = false, off = false;
			std::string ln;
			while (std::getline(f, ln))
			{
				size_t eq = ln.find('=');
				if (eq == std::string::npos || ln[0] == '#' || ln[0] == ';') continue;
				std::string k = ln.substr(0, eq), v = ln.substr(eq + 1);
				auto trim = [](std::string& s) { s.erase(0, s.find_first_not_of(" \t")); s.erase(s.find_last_not_of(" \t\r") + 1); };
				trim(k); trim(v);
				if (k == "url" && !v.empty()) url = true;
				if (k == "enabled" && (v == "0" || v == "false" || v == "off" || v == "no")) off = true;
			}
			return url && !off;
		}

		static pid_t runningPid()
		{
			int pid = atoi(firstLine(lockFile()).c_str());
			return pid > 0 && kill(pid, 0) == 0 ? pid : 0;
		}

		bool running() { return runningPid() != 0; }

		static void run(const char* arg)
		{
			if (running()) return;
			const char* prog = getenv("BANANA_CLOUD");	// (another build of it, for testing)
			if (!prog || !*prog) prog = "/usr/bin/banana-cloud";
			pid_t p = fork();
			if (p == 0)
			{
				if (fork() == 0)
				{
					setsid();
					// its log with the menu's (stderr), at the lowest priority: the menu and the games first
					if (nice(19) < 0) {}
					execl(prog, prog, arg, (char*)nullptr);
					_exit(127);
				}
				_exit(0);
			}
			if (p > 0) waitpid(p, nullptr, 0);
		}

		void start(bool all) { run(all ? "all" : nullptr); }

		static const char* const SYSTEMS[][2] = {
			{ "GBA", "Game Boy Advance" }, { "GB", "Game Boy" }, { "GBC", "Game Boy Color" }, { "NES", "NES" },
			{ "SNES", "Super Nintendo" }, { "MD", "Mega Drive" }, { "PS", "PlayStation" }, { "N64", "Nintendo 64" },
			{ "NDS", "Nintendo DS" },
		};

		static std::string deviceFile(const std::string& dataDir) { return dataDir + "/cloudsync-device.ini"; }

		static void trim(std::string& s) { s.erase(0, s.find_first_not_of(" \t")); s.erase(s.find_last_not_of(" \t\r") + 1); }

		std::vector<System> systems(const std::string& dataDir)
		{
			std::string list;
			bool has = false;
			std::ifstream f(deviceFile(dataDir));
			std::string ln;
			while (std::getline(f, ln))
			{
				size_t eq = ln.find('=');
				if (eq == std::string::npos) continue;
				std::string k = ln.substr(0, eq);
				trim(k);
				if (k == "systems") { list = ln.substr(eq + 1); has = true; }
			}
			for (auto& ch : list) ch = (char)toupper((unsigned char)ch);
			std::vector<std::string> picked;
			std::string cur;
			for (char ch : list + ",")
			{
				if (ch == ',' || ch == ' ' || ch == '\t' || ch == '\r') { if (!cur.empty()) picked.push_back(cur); cur.clear(); }
				else cur += ch;
			}
			std::vector<System> out;
			for (auto& s : SYSTEMS)
			{
				bool on = !has;
				for (auto& p : picked) on |= p == s[0];
				out.push_back({ s[0], s[1], on });
			}
			return out;
		}

		bool setSystems(const std::string& dataDir, const std::vector<System>& all)
		{
			std::string list;
			bool every = true;
			for (auto& s : all)
			{
				every &= s.picked;
				if (s.picked) list += (list.empty() ? "" : ",") + s.id;
			}
			// the other lines (the id banana-cloud made up) stay as they are
			std::string out;
			std::ifstream f(deviceFile(dataDir));
			std::string ln;
			while (std::getline(f, ln))
			{
				size_t eq = ln.find('=');
				std::string k = eq == std::string::npos ? "" : ln.substr(0, eq);
				trim(k);
				if (k != "systems") out += ln + "\n";
			}
			if (!every) out += "systems=" + list + "\n";
			std::string tmp = deviceFile(dataDir) + ".tmp";
			{
				std::ofstream o(tmp);
				if (!(o << out)) return false;
			}
			return rename(tmp.c_str(), deviceFile(dataDir).c_str()) == 0;
		}

		void stop()
		{
			if (pid_t pid = runningPid()) kill(pid, SIGTERM);
		}

		std::string status() { return firstLine(statusFile()); }

		std::string summary(const std::string& line, bool isConfigured)
		{
			if (!isConfigured) return "Not set up";
			std::istringstream in(line);
			std::string state;
			in >> state;
			if (state == "running") { int a = 0, b = 0; in >> a >> b; return b ? std::to_string(a) + " / " + std::to_string(b) : "Syncing"; }
			if (state == "done")
			{
				int sent = 0, got = 0, conflicts = 0, skipped = 0;
				in >> sent >> got >> conflicts >> skipped;
				if (skipped) return std::to_string(skipped) + " skipped";
				if (!sent && !got) return "Up to date";
				std::string s;
				if (sent) s = std::to_string(sent) + " sent";
				if (got) s += (s.empty() ? "" : ", ") + std::to_string(got) + " received";
				return s;
			}
			if (state == "offline") return "No connection";
			if (state == "error") return "Didn't work";
			return "Not yet";
		}
	}
}
