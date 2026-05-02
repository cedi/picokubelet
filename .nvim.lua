-- The esp-rs Xtensa toolchain (channel = "esp", pinned in rust-toolchain.toml
-- and forced by mise via RUSTUP_TOOLCHAIN=esp) ships no rust-analyzer
-- component, so launching rust-analyzer under it dies immediately.
--
-- We split rustup state between the two processes:
--   - rust-analyzer itself: RUSTUP_HOME=~/.rustup, RUSTUP_TOOLCHAIN=stable
--     (the global rustup, where `rustup component add rust-analyzer` lives).
--   - cargo subprocesses spawned by rust-analyzer: RUSTUP_HOME=<project>/.rustup-home,
--     RUSTUP_TOOLCHAIN=esp (the project-local rustup that mise/espup populated
--     with the Xtensa fork). build-std, metadata, and check all run there.

local project_root = vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":p:h")
local project_rustup_home = project_root .. "/.rustup-home"
local global_rustup_home = vim.fn.expand("~/.rustup")

vim.g.rustaceanvim = {
  server = {
    cmd = function()
      return {
        "env",
        "RUSTUP_HOME=" .. global_rustup_home,
        "RUSTUP_TOOLCHAIN=stable",
        "rust-analyzer",
      }
    end,
    default_settings = {
      ["rust-analyzer"] = {
        cargo = {
          target = "xtensa-esp32s3-none-elf",
          extraEnv = {
            RUSTUP_HOME = project_rustup_home,
            RUSTUP_TOOLCHAIN = "esp",
          },
        },
        check = {
          extraEnv = {
            RUSTUP_HOME = project_rustup_home,
            RUSTUP_TOOLCHAIN = "esp",
          },
        },
      },
    },
  },
}
