use super::{Context, Editor};
use crate::{
    compositor::{self, Compositor},
    job::{Callback, Jobs},
    ui::{self, overlay::overlaid, Picker, Popup, Prompt, PromptEvent, Text},
};
use dap::{StackFrame, Thread, ThreadStates};
use helix_core::syntax::config::{DebugArgumentValue, DebugConfigCompletion, DebugTemplate};
use helix_dap::{self as dap, requests::TerminateArguments};
use helix_lsp::block_on;
use helix_view::editor::Breakpoint;

use serde_json::{to_value, Value};
use tui::text::Spans;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use anyhow::{anyhow, bail};

use helix_view::handlers::dap::{breakpoints_changed, jump_to_stack_frame, select_thread_id};

fn thread_picker(
    cx: &mut Context,
    callback_fn: impl Fn(&mut Editor, &dap::Thread) + Send + 'static,
) {
    let debugger = debugger!(cx.editor);

    let future = debugger.threads();
    dap_callback(
        cx.jobs,
        future,
        move |editor, compositor, response: dap::requests::ThreadsResponse| {
            let threads = response.threads;
            if threads.len() == 1 {
                callback_fn(editor, &threads[0]);
                return;
            }
            let debugger = debugger!(editor);

            let thread_states = debugger.thread_states.clone();
            let columns = [
                ui::PickerColumn::new("name", |item: &Thread, _| item.name.as_str().into()),
                ui::PickerColumn::new("state", |item: &Thread, thread_states: &ThreadStates| {
                    thread_states
                        .get(&item.id)
                        .map(|state| state.as_str())
                        .unwrap_or("unknown")
                        .into()
                }),
            ];
            let picker = Picker::new(
                columns,
                0,
                threads,
                thread_states,
                move |cx, thread, _action| callback_fn(cx.editor, thread),
            )
            .with_preview(move |editor, thread| {
                let frames = editor
                    .debug_adapters
                    .get_active_client()
                    .as_ref()?
                    .stack_frames
                    .get(&thread.id)?;
                let frame = frames.first()?;
                let path = frame.source.as_ref()?.path.as_ref()?.as_path();
                let pos = Some((
                    frame.line.saturating_sub(1),
                    frame.end_line.unwrap_or(frame.line).saturating_sub(1),
                ));
                Some((path.into(), pos))
            });
            compositor.push(Box::new(picker));
        },
    );
}

fn get_breakpoint_at_current_line(editor: &mut Editor) -> Option<(usize, Breakpoint)> {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);

    let line = doc.selection(view.id).primary().cursor_line(text);
    let path = doc.path()?;
    editor.breakpoints.get(path).and_then(|breakpoints| {
        let i = breakpoints.iter().position(|b| b.line == line);
        i.map(|i| (i, breakpoints[i].clone()))
    })
}

// -- DAP

fn dap_callback<T, F>(
    jobs: &mut Jobs,
    call: impl Future<Output = helix_dap::Result<serde_json::Value>> + 'static + Send,
    callback: F,
) where
    T: for<'de> serde::Deserialize<'de> + Send + 'static,
    F: FnOnce(&mut Editor, &mut Compositor, T) + Send + 'static,
{
    let callback = Box::pin(async move {
        let json = call.await?;
        let response = serde_json::from_value(json)?;
        let call: Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                callback(editor, compositor, response)
            },
        ));
        Ok(call)
    });

    jobs.callback(callback);
}

pub fn dap_start_impl(
    cx: &mut compositor::Context,
    name: Option<&str>,
    socket: Option<std::net::SocketAddr>,
    params: Option<Vec<std::borrow::Cow<str>>>,
) -> Result<(), anyhow::Error> {
    let doc = doc!(cx.editor);
    let config = doc
        .language_config()
        .and_then(|config| config.debugger.as_ref())
        .ok_or_else(|| anyhow!("No debug adapter available for language"))?;

    let id = cx
        .editor
        .debug_adapters
        .start_client(socket, config)
        .map_err(|e| anyhow!("Failed to start debug client: {}", e))?;

    // TODO: avoid refetching all of this... pass a config in
    let template = match name {
        Some(name) => config.templates.iter().find(|t| t.name == name),
        None => config.templates.first(),
    }
    .ok_or_else(|| anyhow!("No debug config with given name"))?;

    let mut args: HashMap<&str, Value> = HashMap::new();

    for (k, t) in &template.args {
        let mut value = t.clone();
        if let Some(ref params) = params {
            for (i, x) in params.iter().enumerate() {
                let mut param = x.to_string();
                if let Some(DebugConfigCompletion::Advanced(cfg)) = template.completion.get(i) {
                    if matches!(cfg.completion.as_deref(), Some("filename" | "directory")) {
                        param = std::fs::canonicalize(x.as_ref())
                            .ok()
                            .and_then(|pb| pb.into_os_string().into_string().ok())
                            .unwrap_or_else(|| x.to_string());
                    }
                }
                // For param #0 replace {0} in args
                let pattern = format!("{{{}}}", i);
                value = match value {
                    // TODO: just use toml::Value -> json::Value
                    DebugArgumentValue::String(v) => {
                        DebugArgumentValue::String(v.replace(&pattern, &param))
                    }
                    DebugArgumentValue::Array(arr) => DebugArgumentValue::Array(
                        arr.iter().map(|v| v.replace(&pattern, &param)).collect(),
                    ),
                    DebugArgumentValue::Boolean(_) => value,
                };
            }
        }

        match value {
            DebugArgumentValue::String(string) => {
                if let Ok(integer) = string.parse::<usize>() {
                    args.insert(k, to_value(integer).unwrap());
                } else {
                    args.insert(k, to_value(string).unwrap());
                }
            }
            DebugArgumentValue::Array(arr) => {
                args.insert(k, to_value(arr).unwrap());
            }
            DebugArgumentValue::Boolean(bool) => {
                args.insert(k, to_value(bool).unwrap());
            }
        }
    }

    args.insert("cwd", to_value(helix_stdx::env::current_working_dir())?);

    let args = to_value(args).unwrap();

    let callback = |_editor: &mut Editor, _compositor: &mut Compositor, _response: Value| {
        // if let Err(e) = result {
        //     editor.set_error(format!("Failed {} target: {}", template.request, e));
        // }
    };

    let debugger = match cx.editor.debug_adapters.get_client_mut(id) {
        Some(child) => child,
        None => {
            bail!("Failed to get child debugger.");
        }
    };

    match &template.request[..] {
        "launch" => {
            let call = debugger.launch(args);
            dap_callback(cx.jobs, call, callback);
        }
        "attach" => {
            let call = debugger.attach(args);
            dap_callback(cx.jobs, call, callback);
        }
        request => bail!("Unsupported request '{}'", request),
    };

    // TODO: either await "initialized" or buffer commands until event is received
    Ok(())
}

pub fn dap_launch(cx: &mut Context) {
    // TODO: Now that we support multiple Clients, we could run multiple debuggers at once but for now keep this as is
    if cx.editor.debug_adapters.get_active_client().is_some() {
        cx.editor.set_error("Debugger is already running");
        return;
    }

    let doc = doc!(cx.editor);

    let config = match doc
        .language_config()
        .and_then(|config| config.debugger.as_ref())
    {
        Some(c) => c,
        None => {
            cx.editor
                .set_error("No debug adapter available for language");
            return;
        }
    };

    let templates = config.templates.clone();

    let columns = [ui::PickerColumn::new(
        "template",
        |item: &DebugTemplate, _| item.name.as_str().into(),
    )];

    cx.push_layer(Box::new(overlaid(Picker::new(
        columns,
        0,
        templates,
        (),
        |cx, template, _action| {
            if template.completion.is_empty() {
                if let Err(err) = dap_start_impl(cx, Some(&template.name), None, None) {
                    cx.editor.set_error(err.to_string());
                }
            } else {
                let completions = template.completion.clone();
                let name = template.name.clone();
                let callback = Box::pin(async move {
                    let call: Callback =
                        Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                            let prompt = debug_parameter_prompt(completions, name, Vec::new());
                            compositor.push(Box::new(prompt));
                        }));
                    Ok(call)
                });
                cx.jobs.callback(callback);
            }
        },
    ))));
}

pub fn dap_restart(cx: &mut Context) {
    let debugger = match cx.editor.debug_adapters.get_active_client() {
        Some(debugger) => debugger,
        None => {
            cx.editor.set_error("Debugger is not running");
            return;
        }
    };
    if !debugger
        .capabilities()
        .supports_restart_request
        .unwrap_or(false)
    {
        cx.editor
            .set_error("Debugger does not support session restarts");
        return;
    }
    if debugger.starting_request_args().is_none() {
        cx.editor
            .set_error("No arguments found with which to restart the sessions");
        return;
    }

    dap_callback(
        cx.jobs,
        debugger.restart(),
        |editor, _compositor, _resp: ()| editor.set_status("Debugging session restarted"),
    );
}

fn debug_parameter_prompt(
    completions: Vec<DebugConfigCompletion>,
    config_name: String,
    mut params: Vec<String>,
) -> Prompt {
    let completion = completions.get(params.len()).unwrap();
    let field_type = if let DebugConfigCompletion::Advanced(cfg) = completion {
        cfg.completion.as_deref().unwrap_or("")
    } else {
        ""
    };
    let name = match completion {
        DebugConfigCompletion::Advanced(cfg) => cfg.name.as_deref().unwrap_or(field_type),
        DebugConfigCompletion::Named(name) => name.as_str(),
    };
    let default_val = match completion {
        DebugConfigCompletion::Advanced(cfg) => cfg.default.as_deref().unwrap_or(""),
        _ => "",
    }
    .to_owned();

    let completer = match field_type {
        "filename" => |editor: &Editor, input: &str| {
            ui::completers::filename_with_git_ignore(editor, input, false)
        },
        "directory" => |editor: &Editor, input: &str| {
            ui::completers::directory_with_git_ignore(editor, input, false)
        },
        _ => ui::completers::none,
    };

    Prompt::new(
        format!("{}: ", name).into(),
        None,
        completer,
        move |cx, input: &str, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }

            let mut value = input.to_owned();
            if value.is_empty() {
                value = default_val.clone();
            }
            params.push(value);

            if params.len() < completions.len() {
                let completions = completions.clone();
                let config_name = config_name.clone();
                let params = params.clone();
                let callback = Box::pin(async move {
                    let call: Callback =
                        Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                            let prompt = debug_parameter_prompt(completions, config_name, params);
                            compositor.push(Box::new(prompt));
                        }));
                    Ok(call)
                });
                cx.jobs.callback(callback);
            } else if let Err(err) = dap_start_impl(
                cx,
                Some(&config_name),
                None,
                Some(params.iter().map(|x| x.into()).collect()),
            ) {
                cx.editor.set_error(err.to_string());
            }
        },
    )
}

pub fn dap_toggle_breakpoint(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let path = match doc.path() {
        Some(path) => path.clone(),
        None => {
            cx.editor
                .set_error("Can't set breakpoint: document has no path");
            return;
        }
    };
    let text = doc.text().slice(..);
    let line = doc.selection(view.id).primary().cursor_line(text);
    dap_toggle_breakpoint_impl(cx, path, line);
}

pub fn dap_toggle_breakpoint_impl(cx: &mut Context, path: PathBuf, line: usize) {
    // TODO: need to map breakpoints over edits and update them?
    // we shouldn't really allow editing while debug is running though

    let breakpoints = cx.editor.breakpoints.entry(path.clone()).or_default();
    // TODO: always keep breakpoints sorted and use binary search to determine insertion point
    if let Some(pos) = breakpoints
        .iter()
        .position(|breakpoint| breakpoint.line == line)
    {
        breakpoints.remove(pos);
    } else {
        breakpoints.push(Breakpoint {
            line,
            ..Default::default()
        });
    }

    let debugger = debugger!(cx.editor);

    if let Err(e) = breakpoints_changed(debugger, path, breakpoints) {
        cx.editor
            .set_error(format!("Failed to set breakpoints: {}", e));
    }
}

pub fn dap_continue(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.continue_thread(thread_id);

        dap_callback(
            cx.jobs,
            request,
            |editor, _compositor, _response: dap::requests::ContinueResponse| {
                debugger!(editor).resume_application();
            },
        );
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_pause(cx: &mut Context) {
    thread_picker(cx, |editor, thread| {
        let debugger = debugger!(editor);
        let request = debugger.pause(thread.id);
        // NOTE: we don't need to set active thread id here because DAP will emit a "stopped" event
        if let Err(e) = block_on(request) {
            editor.set_error(format!("Failed to pause: {}", e));
        }
    })
}

pub fn dap_step_in(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.step_in(thread_id);

        dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
            debugger!(editor).resume_application();
        });
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_step_out(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.step_out(thread_id);
        dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
            debugger!(editor).resume_application();
        });
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_next(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.next(thread_id);
        dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
            debugger!(editor).resume_application();
        });
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_variables(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if debugger.thread_id.is_none() {
        cx.editor
            .set_status("Cannot access variables while target is running.");
        return;
    }
    let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
        (Some(frame), Some(thread_id)) => (frame, thread_id),
        _ => {
            cx.editor
                .set_status("Cannot find current stack frame to access variables.");
            return;
        }
    };

    let thread_frame = match debugger.stack_frames.get(&thread_id) {
        Some(thread_frame) => thread_frame,
        None => {
            cx.editor
                .set_error(format!("Failed to get stack frame for thread: {thread_id}"));
            return;
        }
    };
    let stack_frame = match thread_frame.get(frame) {
        Some(stack_frame) => stack_frame,
        None => {
            cx.editor.set_error(format!(
                "Failed to get stack frame for thread {thread_id} and frame {frame}."
            ));
            return;
        }
    };

    let frame_id = stack_frame.id;
    let scopes = match block_on(debugger.scopes(frame_id)) {
        Ok(s) => s,
        Err(e) => {
            cx.editor.set_error(format!("Failed to get scopes: {}", e));
            return;
        }
    };

    // TODO: allow expanding variables into sub-fields
    let mut variables = Vec::new();

    let theme = &cx.editor.theme;
    let scope_style = theme.get("ui.linenr.selected");
    let type_style = theme.get("ui.text");
    let text_style = theme.get("ui.text.focus");

    for scope in scopes.iter() {
        // use helix_view::graphics::Style;
        use tui::text::Span;
        let response = block_on(debugger.variables(scope.variables_reference));

        variables.push(Spans::from(Span::styled(
            format!("▸ {}", scope.name),
            scope_style,
        )));

        if let Ok(vars) = response {
            variables.reserve(vars.len());
            for var in vars {
                let mut spans = Vec::with_capacity(5);

                spans.push(Span::styled(var.name.to_owned(), text_style));
                if let Some(ty) = var.ty {
                    spans.push(Span::raw(": "));
                    spans.push(Span::styled(ty.to_owned(), type_style));
                }
                spans.push(Span::raw(" = "));
                spans.push(Span::styled(var.value.to_owned(), text_style));
                variables.push(Spans::from(spans));
            }
        }
    }

    let contents = Text::from(tui::text::Text::from(variables));
    let popup = Popup::new("dap-variables", contents);
    cx.replace_or_push_layer("dap-variables", popup);
}

pub fn dap_terminate(cx: &mut Context) {
    cx.editor.set_status("Terminating debug session...");
    let debugger = debugger!(cx.editor);

    let terminate_arguments = Some(TerminateArguments {
        restart: Some(false),
    });

    let request = debugger.terminate(terminate_arguments);
    dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
        // editor.set_error(format!("Failed to disconnect: {}", e));
        editor.debug_adapters.unset_active_client();
    });
}

pub fn dap_enable_exceptions(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let filters = match &debugger.capabilities().exception_breakpoint_filters {
        Some(filters) => filters.iter().map(|f| f.filter.clone()).collect(),
        None => return,
    };

    let request = debugger.set_exception_breakpoints(filters);

    dap_callback(
        cx.jobs,
        request,
        |_editor, _compositor, _response: dap::requests::SetExceptionBreakpointsResponse| {
            // editor.set_error(format!("Failed to set up exception breakpoints: {}", e));
        },
    )
}

pub fn dap_disable_exceptions(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let request = debugger.set_exception_breakpoints(Vec::new());

    dap_callback(
        cx.jobs,
        request,
        |_editor, _compositor, _response: dap::requests::SetExceptionBreakpointsResponse| {
            // editor.set_error(format!("Failed to set up exception breakpoints: {}", e));
        },
    )
}

// TODO: both edit condition and edit log need to be stable: we might get new breakpoints from the debugger which can change offsets
pub fn dap_edit_condition(cx: &mut Context) {
    if let Some((pos, breakpoint)) = get_breakpoint_at_current_line(cx.editor) {
        let path = match doc!(cx.editor).path() {
            Some(path) => path.clone(),
            None => return,
        };
        let callback = Box::pin(async move {
            let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
                let mut prompt = Prompt::new(
                    "condition:".into(),
                    None,
                    ui::completers::none,
                    move |cx, input: &str, event: PromptEvent| {
                        if event != PromptEvent::Validate {
                            return;
                        }

                        let breakpoints = &mut cx.editor.breakpoints.get_mut(&path).unwrap();
                        breakpoints[pos].condition = match input {
                            "" => None,
                            input => Some(input.to_owned()),
                        };

                        let debugger = debugger!(cx.editor);

                        if let Err(e) = breakpoints_changed(debugger, path.clone(), breakpoints) {
                            cx.editor
                                .set_error(format!("Failed to set breakpoints: {}", e));
                        }
                    },
                );
                if let Some(condition) = breakpoint.condition {
                    prompt.insert_str(&condition, editor)
                }
                compositor.push(Box::new(prompt));
            }));
            Ok(call)
        });
        cx.jobs.callback(callback);
    }
}

pub fn dap_edit_log(cx: &mut Context) {
    if let Some((pos, breakpoint)) = get_breakpoint_at_current_line(cx.editor) {
        let path = match doc!(cx.editor).path() {
            Some(path) => path.clone(),
            None => return,
        };
        let callback = Box::pin(async move {
            let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
                let mut prompt = Prompt::new(
                    "log-message:".into(),
                    None,
                    ui::completers::none,
                    move |cx, input: &str, event: PromptEvent| {
                        if event != PromptEvent::Validate {
                            return;
                        }

                        let breakpoints = &mut cx.editor.breakpoints.get_mut(&path).unwrap();
                        breakpoints[pos].log_message = match input {
                            "" => None,
                            input => Some(input.to_owned()),
                        };

                        let debugger = debugger!(cx.editor);
                        if let Err(e) = breakpoints_changed(debugger, path.clone(), breakpoints) {
                            cx.editor
                                .set_error(format!("Failed to set breakpoints: {}", e));
                        }
                    },
                );
                if let Some(log_message) = breakpoint.log_message {
                    prompt.insert_str(&log_message, editor);
                }
                compositor.push(Box::new(prompt));
            }));
            Ok(call)
        });
        cx.jobs.callback(callback);
    }
}

pub fn dap_switch_thread(cx: &mut Context) {
    thread_picker(cx, |editor, thread| {
        block_on(select_thread_id(editor, thread.id, true));
    })
}
pub fn dap_switch_stack_frame(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let thread_id = match debugger.thread_id {
        Some(thread_id) => thread_id,
        None => {
            cx.editor.set_error("No thread is currently active");
            return;
        }
    };

    let frames = debugger.stack_frames[&thread_id].clone();

    let columns = [ui::PickerColumn::new("frame", |item: &StackFrame, _| {
        item.name.as_str().into() // TODO: include thread_states in the label
    })];
    let picker = Picker::new(columns, 0, frames, (), move |cx, frame, _action| {
        let debugger = debugger!(cx.editor);
        // TODO: this should be simpler to find
        let pos = debugger.stack_frames[&thread_id]
            .iter()
            .position(|f| f.id == frame.id);
        debugger.active_frame = pos;

        let frame = debugger.stack_frames[&thread_id]
            .get(pos.unwrap_or(0))
            .cloned();
        if let Some(frame) = &frame {
            jump_to_stack_frame(cx.editor, frame);
        }
    })
    .with_preview(move |_editor, frame| {
        frame
            .source
            .as_ref()
            .and_then(|source| source.path.as_ref())
            .map(|path| {
                (
                    path.as_path().into(),
                    Some((
                        frame.line.saturating_sub(1),
                        frame.end_line.unwrap_or(frame.line).saturating_sub(1),
                    )),
                )
            })
    });
    cx.push_layer(Box::new(picker))
}
