//! Google Gemini adapter for the rhema [`ModelProvider`](rhema_core::ModelProvider)
//! seam: speaks the Generative Language API directly (streaming SSE, native
//! function calling), maps it onto `ModelRequest`/`ModelOutput`/`Event`.
//!
//! Auth is by API key; the default model target is Gemini Flash.
