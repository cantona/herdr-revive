// herdr-revive: what the last snapshot holds, what the next core start
// would relaunch, and whether the shell hook is in place. Every fact comes
// from one `herdr-revive --host core status`; buttons run the matching
// command, then read status again. No timer.
import { View, div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { exec, hc, machines, props, closeOverlay, theme, notify } from "herdr";

// Looked for where the rc line looks: the app's PATH, then ~/.local/bin and
// ~/.cargo/bin, which an app started from the desktop often does not have.
// The two are appended to PATH as well, so the programs herdr-revive starts
// (herdr, hc, herdr-app) resolve by bare name; same as external::LOOKUP.
const FIND = 'PATH="${PATH:+$PATH:}$HOME/.local/bin:$HOME/.cargo/bin"; export PATH; for f in "$(command -v herdr-revive 2>/dev/null)" "$HOME/.local/bin/herdr-revive" "$HOME/.cargo/bin/herdr-revive"; do [ -x "$f" ] && exec "$f" "$@"; done; echo "cannot run herdr-revive: not on PATH, in ~/.local/bin or in ~/.cargo/bin" >&2; exit 127';
const NOT_FOUND = "cannot run herdr-revive";
const run = (args, options) => exec("sh", ["-c", FIND, "herdr-revive", ...args], options);
const revive = (args, options) => run(["--host", "core", ...args], options);
const partialText = (p) =>
  [p.space && `space ${p.space}`, p.terminals?.length && `terminals ${p.terminals.join(" ")}`]
    .filter(Boolean).join(", ") || "nothing";
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
// The error main prints comes last; warnings about single panes come first.
const summary = (text) => {
  const lines = (text ?? "").split("\n").map((l) => l.trim()).filter(Boolean);
  const errors = lines.filter((l) => l.startsWith("herdr-revive:") && !l.startsWith("herdr-revive: warning:"));
  return errors[errors.length - 1] ?? lines.find((l) => l.startsWith("error:")) ?? lines.slice(0, 3).join("\n");
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
    this.machine = null;
    this.machineList = [null];
    this.plan = null;
    this.loadMachines(cx);
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

  loadMachines(cx) {
    cx.spawn(async (cx) => {
      let names = [];
      let temporary = new Set();
      try {
        names = machines().map((m) => m.name).filter((name) => name !== null);
        const r = await hc(["machine", "ls", "--json"]);
        if (r.code !== 0) throw new Error(summary(r.stderr || r.stdout) || `hc exited with code ${r.code}`);
        temporary = new Set((JSON.parse(r.stdout).machines ?? []).filter((m) => m.temporary).map((m) => m.name));
      } catch (e) {
        notify({ title: "herdr-revive", body: `Cannot list machines completely: ${String(e)}`, level: "warning" });
      }
      this.machineList = [null, ...names.filter((name) => !temporary.has(name))];
      cx.notify();
    });
  }

  migrate(cx, to) {
    if (this.busy || this.reading) return;
    const args = ["migrate", "--to", to, "--json", ...(this.machine ? ["--machine", this.machine] : [])];
    this.busy = `migrate-${to}`;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const r = await run([...args, "--dry-run"], { timeout: 600000 });
        if (r.code !== 0) {
          notify({ title: "herdr-revive migrate failed", body: summary(r.stderr || r.stdout) || `exited with code ${r.code}`, level: "error" });
          return;
        }
        this.plan = { args, report: JSON.parse(r.stdout) };
      } catch (e) {
        notify({ title: "herdr-revive migrate failed", body: String(e), level: "error" });
      } finally {
        this.busy = null;
        cx.notify();
      }
    });
  }

  copy(cx) {
    if (this.busy || this.reading || !this.plan) return;
    // The --yes run plans again; --only keeps it to what this plan showed.
    const sources = this.plan.report.lines.filter((l) => l.status === "create").map((l) => l.source);
    if (!sources.length) {
      notify({ title: "herdr-revive", body: "Nothing to copy.", level: "info" });
      return;
    }
    const { args } = this.plan;
    const { machine, to } = this.plan.report;
    this.plan = null;
    this.busy = "copying";
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const r = await run([...args, ...sources.map((source) => `--only=${source}`), "--yes"], { timeout: 600000 });
        // A failed --yes still prints the report with what was applied.
        let done = null;
        try {
          done = JSON.parse(r.stdout);
        } catch (_e) {
          done = null;
        }
        // What the run warned about without stopping (a copy another machine
        // could not record, a pane the capture could not read in full).
        const warn = () => {
          const warnings = done?.warnings ?? [];
          if (warnings.length) notify({ title: "herdr-revive migrate warnings", body: warnings.join("\n"), level: "warning" });
        };
        if (r.code !== 0) {
          // applied is null when another machine's result could not be read.
          const applied = Array.isArray(done?.applied) ? ` ${done.applied.length} of ${done.summary.create} copied before it stopped.` : "";
          notify({ title: "herdr-revive migrate stopped", body: `${summary(r.stderr || r.stdout) || `exited with code ${r.code}`}${applied}`, level: "error" });
          warn();
          return;
        }
        if (!done) throw new Error("unreadable migrate output");
        notify({ title: "herdr-revive", body: `Copied ${done.applied.length} of ${done.summary.create}.`, level: "info" });
        warn();
      } catch (e) {
        // The app stops the local run at its limit. A copy into another
        // machine's TUI runs there through `hc exec`, which goes on; a copy
        // into the app is built from here and stops with the run.
        const stopped = /took longer than/.test(String(e));
        if (stopped && to === "tui" && machine) {
          notify({ title: "herdr-revive migrate", body: `The copy may still finish on ${machine}; open the plan again to see what was copied.`, level: "warning" });
        } else if (stopped && to === "app") {
          notify({ title: "herdr-revive migrate", body: "The copy stopped part way. Close what it left in the app before copying again, then open the plan again.", level: "warning" });
        } else {
          notify({ title: "herdr-revive migrate failed", body: String(e), level: "error" });
        }
      } finally {
        this.busy = null;
        cx.notify();
      }
    });
  }

  chip(id, text, selected, action) {
    const t = theme();
    return h_flex().id(id).h(24).when(Boolean(this.busy || this.reading), (el) => el.opacity(0.6)).px(10).items_center().rounded(12)
      .border_1().border_color(selected ? t.solid : t.input).bg(selected ? t.solid : t.raised)
      .text_color(selected ? t.solid_foreground : t.foreground).text_size(12).cursor_pointer()
      .on_click((_e, cx) => action(cx))
      .child(text);
  }

  planView(t) {
    const { report } = this.plan;
    const where = (l) => (l.machine ? `${l.machine}: ` : "");
    const items = [div().text_color(t.muted).child(report.to === "app"
      ? "Copy the TUI session into the app:" : "Copy the app's spaces into the TUI:")
      .child(` (${report.machine ?? "this computer"})`)];
    for (const l of report.lines) {
      const left = `${where(l)}${l.source} -> ${l.name || "(automatic name)"}  ${l.tabs} tabs, ${l.panes} panes, ${l.agents} agents`;
      const right = l.status === "skip" ? `skip: ${l.reason}` : l.status === "unchanged" && l.copy ? `unchanged, copy ${l.copy}` : l.status;
      items.push(row(t, `plan-${l.source}`, left, right, l.status === "create" ? t.done : t.muted));
      if (l.denied) items.push(div().pl(12).text_color(t.dim).child(`${l.denied} command(s) not allowed by the policy open a shell.`));
      if (l.mod_blocks) items.push(div().pl(12).text_color(t.dim).child(`${l.mod_blocks} mod pane(s) are left out.`));
      if (l.live_agents.length) items.push(div().pl(12).text_color(t.blocked).child(
        `Still live in the TUI: ${l.live_agents.join(" ")}. Both copies resume the same session until one is closed.`));
      if (l.partial) items.push(div().pl(12).text_color(t.blocked).child(
        `An earlier copy stopped part way (${partialText(l.partial)}); close it first.`));
      // Notes are plan sentences ("an earlier run at ..."); older reports have none.
      for (const note of l.notes ?? []) items.push(div().pl(12).text_color(t.blocked).child(
        `${note.charAt(0).toUpperCase()}${note.slice(1)}.`));
    }
    for (const warning of report.warnings ?? []) items.push(div().pt(4).text_color(t.blocked).child(`Warning: ${warning}`));
    if (report.summary.ratios_lost) items.push(div().pt(4).text_color(t.dim).child("Split sizes are not copied: the app's splits come out even."));
    items.push(h_flex().gap(8).pt(8).justify_end()
      .child(this.button("plan-cancel", "Cancel", false, (cx) => { this.plan = null; cx.notify(); }))
      .child(this.button("plan-copy", report.summary.create ? `Copy ${report.summary.create}` : "Nothing to copy", true, (cx) => this.copy(cx))));
    return items;
  }

  migration(t) {
    const items = [label(t, "Migrate")];
    if (this.busy === "copying") return [...items, div().text_color(t.muted).child("Copying...")];
    if (this.busy?.startsWith("migrate-")) return [...items, div().text_color(t.muted).child("Planning...")];
    if (this.plan) return [...items, ...this.planView(t)];
    items.push(h_flex().gap(6).pt(2).children(this.machineList.map((name) =>
      this.chip(`machine-${name ?? "here"}`, name ?? "This computer", name === this.machine,
        (cx) => { if (this.busy || this.reading) return; this.machine = name; cx.notify(); }))));
    items.push(h_flex().gap(8).pt(8)
      .child(this.button("migrate-app", "Copy TUI session here", false, (cx) => this.migrate(cx, "app")))
      .child(this.button("migrate-tui", "Copy app spaces to TUI", false, (cx) => this.migrate(cx, "tui"))));
    return items;
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
    return v_flex().gap(2).children([...items, ...this.migration(t), ...this.setup(t, d)]);
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
