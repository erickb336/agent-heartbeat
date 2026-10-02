//! A per-user LaunchAgent for the tracker. The tool prints it; it never installs or loads it.

pub const LABEL: &str = "io.github.erickb336.agent-heartbeat";

pub fn plist(exe: &str) -> String {
    let exe = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
  </array>
  <!-- Start at login. Restart only after a crash, at most once every 30 s. -->
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>30</integer>
  <!-- A background job: low CPU and I/O priority. -->
  <key>ProcessType</key>
  <string>Background</string>
  <key>LowPriorityIO</key>
  <true/>
  <key>Nice</key>
  <integer>10</integer>
  <!-- The tool keeps its own bounded log in ~/Library/Logs/agent-heartbeat. -->
  <key>StandardOutPath</key>
  <string>/dev/null</string>
  <key>StandardErrorPath</key>
  <string>/dev/null</string>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn plist_runs_the_binary_without_root() {
        let p = super::plist("/Users/x/.cargo/bin/agent-heartbeat");
        assert!(p.contains("<string>/Users/x/.cargo/bin/agent-heartbeat</string>"));
        assert!(p.contains("<string>run</string>"));
        assert!(!p.contains("UserName"), "a per-user agent never sets a user");
    }
}
