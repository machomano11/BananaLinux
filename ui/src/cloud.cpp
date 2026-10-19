#include "cloud.h"
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

		void start(bool all)
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
					execl(prog, prog, all ? "all" : nullptr, (char*)nullptr);
					_exit(127);
				}
				_exit(0);
			}
			if (p > 0) waitpid(p, nullptr, 0);
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
