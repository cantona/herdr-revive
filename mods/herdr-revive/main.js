// herdr-revive: what the last snapshot holds, what the next core start
// would relaunch, and whether the shell hook is in place. Every fact comes
// from one `herdr-revive --host core status`; buttons run the matching
// command, then read status again. No timer.
import { View, div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { exec, props, closeOverlay, theme, notify } from "herdr";

// Looked for where the rc line looks: the app's PATH, then ~/.local/bin and
// ~/.cargo/bin, which an app started from the desktop often does not have.
const FIND = 'for f in "$(command -v herdr-revive 2>/dev/null)" "$HOME/.local/bin/herdr-revive" "$HOME/.cargo/bin/herdr-revive"; do [ -x "$f" ] && exec "$f" "$@"; done; echo "cannot run herdr-revive: not on PATH, in ~/.local/bin or in ~/.cargo/bin" >&2; exit 127';
const NOT_FOUND = "cannot run herdr-revive";
const revive = (args, options) => exec("sh", ["-c", FIND, "herdr-revive", "--host", "core", ...args], options);
const INSTALL = "cargo install --locked --git https://github.com/cantona/herdr-revive";
const RC_INSTALL = "herdr-revive install";
const DECISION = {
  candidate: "will relaunch",
  no_command: "nothing to relaunch",
  missing_pane: "pane is gone",
  identity_mismatch: "moved to another tab",
  denied_by_policy: "not in allowed_programs",
};
const when = (ms) => new Date(ms).toLocaleString();
const summary = (text) => {
  const lines = (text ?? "").split("\n").map((l) => l.trim()).filter(Boolean);
  return lines.find((l) => l.startsWith("error:")) ?? lines.slice(0, 3).join("\n");
};
const tooOld = (text) => /unexpected argument '--host'|unrecognized subcommand/.test(text ?? "");
const label = (t, text) => div().pt(12).pb(4).text_size(11.5).text_color(t.dim).child(text);
const row = (t, id, left, right, color) => h_flex().id(id).gap(8).pt(2).pb(2).items_center()
  .child(div().flex_1().truncate().text_color(t.foreground).child(left))
  .child(div().text_color(color ?? t.muted).child(right));

export default class Revive extends View {
  init(_props, cx) {
    this.handle = cx.focus_handle();
    this.handle.focus();
    this.state = { loading: true };
    this.busy = null;
    this.reading = false;
    this.confirming = false;
    this.refresh(cx);
  }

  async read() {
    let r;
    try {
      r = await revive(["status"], { timeout: 15000 });
    } catch (e) {
      this.state = { error: String(e) };
      return;
    }
    const output = r.stderr || r.stdout;
    if (r.code === 127 && output.includes(NOT_FOUND)) {
      this.state = { install: "herdr-revive could not be found.", error: summary(output) };
      return;
    }
    if (r.code !== 0) {
      const error = summary(output) || `herdr-revive exited with code ${r.code}`;
      this.state = tooOld(output) ? { install: "herdr-revive is too old for this dialog.", error } : { error };
      return;
    }
    try {
      const body = JSON.parse(r.stdout);
      this.state = body.status === "operation_in_progress" ? { locked: true } : { data: body.result ?? {} };
    } catch (e) {
      this.state = { error: `unreadable status output: ${String(e)}` };
    }
  }

  refresh(cx) {
    if (this.busy || this.reading) return;
    this.reading = true;
    this.state = { loading: true };
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        await this.read();
      } finally {
        this.reading = false;
        cx.notify();
      }
    });
  }

  act(cx, name, args) {
    if (this.busy || this.reading) return;
    this.busy = name;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        let r;
        try {
          r = await revive(args, { timeout: 120000 });
        } catch (e) {
          r = { code: 1, stderr: String(e), stdout: "" };
        }
        if (r.code !== 0) notify({ title: `herdr-revive ${name} failed`, body: summary(r.stderr || r.stdout) || `exited with code ${r.code}`, level: "error" });
        this.reading = true;
        cx.notify();
        await this.read();
      } finally {
        this.reading = false;
        this.busy = null;
        cx.notify();
      }
    });
  }

  close() {
    closeOverlay(props().overlay);
  }

  button(id, text, primary, action) {
    const t = theme();
    return h_flex().id(id).h(30).px(12).items_center().rounded(8)
      .border_1().border_color(primary ? t.solid : t.input).bg(primary ? t.solid : t.raised)
      .text_color(primary ? t.solid_foreground : t.foreground).text_size(12.5).font_medium().cursor_pointer()
      .when(Boolean(this.busy || this.reading), (el) => el.opacity(0.6))
      .hover((s) => s.opacity(0.88))
      .on_click((_e, cx) => action(cx))
      .child(this.busy === id ? "..." : text);
  }

  setup(t, d) {
    const command = (text) => div().pl(12).text_color(t.muted).child(text);
    const install = d.install ?? {};
    const cfg = d.config ?? {};
    const hook = `Shell hook in ${install.rc_file ?? "the rc file"}`;
    const items = [label(t, "Setup")];
    if (install.rc_error) {
      items.push(row(t, "rc", hook, "unreadable", t.blocked));
      items.push(div().pl(12).text_color(t.blocked).child(install.rc_error));
    } else if (!install.rc_installed) {
      items.push(row(t, "rc", hook, "missing", t.blocked));
      items.push(command(RC_INSTALL));
    } else if (!install.rc_up_to_date) {
      items.push(row(t, "rc", hook, "update needed", t.blocked));
      items.push(command(RC_INSTALL));
    } else {
      items.push(row(t, "rc", hook, "installed", t.done));
    }
    items.push(row(t, "cfg", `Config ${cfg.dir ?? ""}/config.toml`, cfg.present ? "present" : "defaults", cfg.present ? t.done : t.muted));
    items.push(row(t, "auto", "auto_save / auto_restore", `${cfg.auto_save ? "on" : "off"} / ${cfg.auto_restore ? "on" : "off"}`));
    items.push(row(t, "allow", "allowed_programs", (cfg.allowed_programs ?? []).join(", ") || "none"));
    return items;
  }

  details(d) {
    const t = theme();
    const colors = { candidate: t.done, denied_by_policy: t.blocked, missing_pane: t.dim, identity_mismatch: t.dim, no_command: t.muted };
    const items = [label(t, "Last snapshot")];
    if (d.latest) items.push(row(t, "latest", when(d.latest.created_ms), `${d.latest.commands} of ${d.latest.panes} panes with a command`));
    else items.push(div().text_color(t.muted).child("No snapshot yet. Run an allowed program in a pane, or press Save now."));
    const panes = d.panes ?? [];
    if (panes.length) {
      items.push(label(t, "Next core start"));
      for (const p of panes) {
        items.push(row(t, `pane-${p.pane_id}`, `${p.terminal_id ?? p.pane_id}  ${p.program ?? "shell"}  ${p.cwd ?? ""}`, DECISION[p.decision] ?? p.decision, colors[p.decision]));
      }
    }
    const pending = d.recovery?.pending;
    if (pending) {
      items.push(label(t, "Recovery"));
      items.push(div().text_color(t.blocked).child(`A relaunch stopped in state ${pending.state}; saves and relaunches wait until it is acknowledged.`));
      items.push(h_flex().pt(6).child(this.button("ack", "Acknowledge without retry", false, (cx) => this.act(cx, "ack", ["recovery", "acknowledge", pending.generation]))));
    }
    return v_flex().gap(2).children([...items, ...this.setup(t, d)]);
  }

  render(cx) {
    const t = theme();
    const s = this.state;
    let body;
    if (s.loading) body = div().text_color(t.muted).child("Reading...");
    else if (s.install) body = v_flex().gap(6)
      .child(div().text_color(t.blocked).child(s.install))
      .child(div().text_color(t.muted).child("Install the current version, then open this dialog again:"))
      .child(div().text_color(t.foreground).child(INSTALL))
      .child(div().text_size(11.5).text_color(t.dim).child(s.error));
    else if (s.locked) body = div().text_color(t.muted).child("Another herdr-revive operation is running. Refresh in a moment.");
    else if (s.error) body = div().text_color(t.blocked).child(s.error);
    else body = this.details(s.data);
    return v_flex().id("revive").track_focus(this.handle)
      .on_key_down((e, cx) => { if (e.key === "escape") { this.close(); cx.stop_propagation(); } })
      .w_full().p(18).gap(4).rounded(8).bg(t.popover).text_size(13)
      .child(h_flex().items_center()
        .child(div().flex_1().text_size(15).font_semibold().text_color(t.foreground).child("Herdr Revive"))
        .child(div().text_size(11).text_color(t.dim).child("Esc")))
      // The dialog is as tall as its content; a bounded body scrolls instead.
      .child(div().max_h(460).overflow_y_scrollbar().child(body))
      // Relaunching types each command into its pane, after anything typed
      // there and not entered: so a second click, with that said first.
      .when(this.confirming, (el) => el.child(div().pt(10).text_color(t.blocked).child(
        `Relaunch ${this.candidates()} pane(s)? Each command is typed into its pane's prompt, so a line typed there and not entered would run with it.`)))
      .child(h_flex().pt(12).gap(8).justify_end()
        .child(this.button("refresh", "Refresh", false, (cx) => { this.confirming = false; this.refresh(cx); }))
        .child(this.button("save", "Save now", false, (cx) => this.act(cx, "save", ["save"])))
        .when(this.confirming, (el) => el.child(this.button("cancel", "Cancel", false, (cx) => { this.confirming = false; cx.notify(); })))
        .child(this.confirming
          ? this.button("restore", "Relaunch", true, (cx) => { this.confirming = false; this.act(cx, "restore", ["restore", "--rehydrate"]); })
          : this.button("restore", "Relaunch idle panes", true, (cx) => {
            if (this.candidates() === 0) {
              notify({ title: "herdr-revive", body: "No pane is set to relaunch.", level: "info" });
              return;
            }
            this.confirming = true;
            cx.notify();
          })));
  }

  candidates() {
    return (this.state?.data?.panes ?? []).filter((p) => p.decision === "candidate").length;
  }
}
