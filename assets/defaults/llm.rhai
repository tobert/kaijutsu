// Kaijutsu LLM Provider Configuration
//
// This file defines the available LLM providers and their settings.
// API keys are read from environment variables, never stored here.
//
// Provider Configuration:
//   enabled       - Whether this provider is available
//   api_key_env   - Environment variable containing the API key
//   base_url      - Optional custom endpoint (for local/proxy servers)
//   default_model - Model to use when not specified
//   default_tools - Tool filter for this provider:
//                   { type: "all" }                     - All registered tools
//                   { type: "allow", tools: [...] }    - Only these tools
//                   { type: "deny", tools: [...] }     - All except these tools

// ============================================================================
// Default Provider
// ============================================================================

// Which provider to use by default (must be enabled below)
let default_provider = "anthropic";

// ============================================================================
// Provider Configurations
// ============================================================================

let providers = #{

    // Anthropic (Claude)
    // Requires: ANTHROPIC_API_KEY environment variable
    anthropic: #{
        enabled: true,
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: "claude-haiku-4-5-20251001",
        max_output_tokens: 64000,
        default_tools: #{ type: "all" },
    },

    // Google Gemini
    // Requires: GEMINI_API_KEY environment variable
    gemini: #{
        enabled: false,
        api_key_env: "GEMINI_API_KEY",
        default_model: "gemini-2.0-flash",
        max_output_tokens: 16384,
        default_tools: #{ type: "all" },
    },

    // OpenAI
    // Requires: OPENAI_API_KEY environment variable
    openai: #{
        enabled: false,
        api_key_env: "OPENAI_API_KEY",
        default_model: "gpt-4o",
        max_output_tokens: 16384,
        default_tools: #{ type: "all" },
    },

    // Ollama (Local)
    // No API key required - connects to local server
    ollama: #{
        enabled: false,
        base_url: "http://localhost:11434",
        default_model: "qwen2.5-coder:7b",
        // Local models may not handle all tools safely - be selective
        default_tools: #{ type: "deny", tools: ["bash", "shell"] },
    },

};

// ============================================================================
// Model Aliases (Optional)
// ============================================================================
// Short names for commonly used model configurations

let model_aliases = #{
    // Fast, cheap models for simple tasks
    "fast": #{ provider: "anthropic", model: "claude-haiku-4-5-20251001" },
    "cheap": #{ provider: "anthropic", model: "claude-haiku-4-5-20251001" },

    // Balanced models for general use
    "balanced": #{ provider: "anthropic", model: "claude-sonnet-4-20250514" },
    "default": #{ provider: "anthropic", model: "claude-sonnet-4-20250514" },

    // Powerful models for complex tasks
    "smart": #{ provider: "anthropic", model: "claude-opus-4-5-20251101" },
    "best": #{ provider: "anthropic", model: "claude-opus-4-5-20251101" },

    // Local models (when ollama is enabled)
    "local": #{ provider: "ollama", model: "qwen2.5-coder:7b" },
    "offline": #{ provider: "ollama", model: "qwen2.5-coder:7b" },
};

// ============================================================================
// Streaming Configuration
// ============================================================================

let streaming = #{
    // Enable streaming responses (recommended)
    enabled: true,

    // Buffer size for streaming chunks
    buffer_size: 1024,

    // Timeout for streaming responses (milliseconds)
    timeout_ms: 120000,
};

// ============================================================================
// Rate Limiting (Optional)
// ============================================================================

let rate_limits = #{
    // Requests per minute (0 = unlimited)
    requests_per_minute: 0,

    // Tokens per minute (0 = unlimited)
    tokens_per_minute: 0,

    // Cooldown between requests (milliseconds)
    min_request_interval_ms: 0,
};
