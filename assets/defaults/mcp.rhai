// Kaijutsu MCP Server Configuration
//
// Defines MCP servers to launch automatically when a kernel starts.
// Each server entry specifies how to connect (stdio subprocess or HTTP).
//
// Server fields:
//   enabled    - Whether to launch this server (default: true)
//   transport  - "stdio" (default) or "streamable_http"
//   command    - Executable path (stdio only)
//   args       - Arguments array (stdio only)
//   env        - Environment variables map
//   cwd        - Working directory (stdio only)
//   url        - Server URL (streamable_http only)
//
// Example:
//   kaish: #{ command: "kaish-mcp" },
//   my_tool: #{ command: "/path/to/tool", args: ["--stdio"] },

let servers = #{};
