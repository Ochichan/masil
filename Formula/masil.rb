class Masil < Formula
  desc "Terminal multiplexer with native coding-agent management"
  homepage "https://github.com/Ochichan/masil"
  # Stable release metadata is inserted here.
  license "MIT"
  head "https://github.com/Ochichan/masil.git", branch: "main"

  depends_on "autoconf" => :build
  depends_on "automake" => :build
  depends_on "m4" => :build
  depends_on "pkgconf" => :build
  depends_on "python@3.13" => :build
  depends_on "rust" => :build
  depends_on "jemalloc"
  depends_on "libevent"
  depends_on "ncurses"
  depends_on "utf8proc"

  on_linux do
    depends_on "bison" => :build
  end

  def fetch
    system "cargo", "fetch", "--manifest-path=agent/Cargo.toml", *std_cargo_fetch_args
  end

  def install
    %w[libevent ncurses utf8proc jemalloc].each do |dependency|
      ENV.prepend_path "PKG_CONFIG_PATH", formula_opt_lib(dependency)/"pkgconfig"
    end
    ENV.prepend_path "ACLOCAL_PATH", formula_opt_share("pkgconf")/"aclocal"

    python = formula_opt_bin("python@3.13")/"python3.13"
    jobs = [ENV.make_jobs, 32].min
    system python, "scripts/build.py", "--jobs", jobs

    ENV["CARGO_NET_OFFLINE"] = "true"
    system python, "scripts/build.py", "--agent", "--jobs", jobs

    bin.install "bin/masil", "bin/masil-agent"
    man1.install "core/tmux.1" => "masil.1"

    pkgshare.install "LICENSE", "NOTICE"
    (pkgshare/"core").install "core/COPYING", "core/UPSTREAM.json"
    (pkgshare/"core/compat/yyjson").install "core/compat/yyjson/LICENSE"
    (pkgshare/"agent/src/manifests").install "agent/src/manifests/LICENSE"
  end

  test do
    socket_dir = Pathname(Dir.mktmpdir("m", "/tmp"))
    socket_dir.chmod 0700
    socket = socket_dir/"s"
    home = testpath/"home"
    cache_home = testpath/"cache"
    config_home = testpath/"config"
    data_home = testpath/"data"
    runtime_dir = testpath/"runtime"
    state_home = testpath/"state"
    [home, cache_home, config_home, data_home, runtime_dir, state_home].each(&:mkpath)
    runtime_dir.chmod 0700

    begin
      with_env(HOME:                home,
               MASIL_AGENT_SOCKET:  nil,
               MASIL_BRIDGE_SOCKET: "off",
               TMUX:                nil,
               TMUX_PANE:           nil,
               TMUX_TMPDIR:         socket_dir,
               XDG_CACHE_HOME:      cache_home,
               XDG_CONFIG_HOME:     config_home,
               XDG_DATA_HOME:       data_home,
               XDG_RUNTIME_DIR:     runtime_dir,
               XDG_STATE_HOME:      state_home) do
        assert_match "tmux next-3.9", shell_output("#{bin}/masil -V")
        agent_version = shell_output("#{bin}/masil-agent --version").strip
        assert_match(/\Amasil-agent \d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?\z/, agent_version)

        system bin/"masil", "-f", File::NULL, "-S", socket,
               "new-session", "-d", "-s", "brewtest"
        sessions = shell_output("#{bin}/masil -f #{File::NULL} -S #{socket} list-sessions -F '\#{session_name}'")
        assert_equal "brewtest\n", sessions

        providers = JSON.parse(shell_output("#{bin}/masil-agent agent providers")).fetch("providers")
        assert providers.any? { |provider| provider["id"] == "codex" }
      ensure
        quiet_system bin/"masil", "-f", File::NULL, "-S", socket, "kill-server"
      end
    ensure
      remove_entry socket_dir if socket_dir.exist?
    end
  end
end
