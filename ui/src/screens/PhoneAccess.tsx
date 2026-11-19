import { useCallback, useEffect, useState } from "react";
import { rpc, ui, usePluginI18n } from "../host/runtime";

type Way = "local" | "tunnel" | "tailscale";
type LoginPhase = "idle" | "sending" | "code_sent" | "verifying" | "signed_in" | "failed";
type Access = {
  way: Way;
  lan: boolean;
  port: number;
  tunnel_account: boolean;
  tailscale_installed: boolean;
  login: { phase: LoginPhase; email?: string | null; error?: string | null };
};

const message = (reason: unknown) => (reason instanceof Error ? reason.message : String(reason));

/**
 * How phones reach OpenMausBot: one way in from outside (tunnel or Tailscale)
 * plus the local network. Saving restarts the server; pairing itself happens
 * in OpenMausBot's own Settings, which then shows a QR the phone app accepts.
 */
export function PhoneAccess({ onClose }: { onClose: () => void }) {
  const { t } = usePluginI18n();
  const { Button } = ui();
  const [access, setAccess] = useState<Access | null>(null);
  const [way, setWay] = useState<Way>("local");
  const [lan, setLan] = useState(false);
  const [port, setPort] = useState("8799");
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);

  const load = useCallback((adopt: boolean) => {
    rpc<Access>("access").then((reply) => {
      setAccess(reply);
      if (adopt) {
        setWay(reply.way);
        setLan(reply.lan);
        setPort(String(reply.port));
        if (reply.login.email) setEmail(reply.login.email);
      }
    }, (reason) => setError(message(reason)));
  }, []);
  useEffect(() => load(true), [load]);
  const phase = access?.login.phase ?? "idle";
  // The sign-in moves on its own thread: follow it while it runs.
  useEffect(() => {
    if (phase !== "sending" && phase !== "verifying") return;
    const timer = window.setInterval(() => load(false), 1000);
    return () => window.clearInterval(timer);
  }, [phase, load]);

  const call = (op: string, params: unknown) => {
    setError(null);
    return rpc<Access["login"]>(op, params).then(
      () => load(false),
      (reason) => setError(message(reason)),
    );
  };
  const save = () => {
    setSaving(true);
    setSaved(false);
    setError(null);
    rpc<Access & { error?: string }>("access.set", { way, lan, port: Number(port) })
      .then((reply) => {
        if (reply?.error) return setError(reply.error);
        setAccess(reply);
        setSaved(true);
      }, (reason) => setError(message(reason)))
      .finally(() => setSaving(false));
  };

  const signedIn = access?.tunnel_account ?? false;
  const needsSignIn = way === "tunnel" && !signedIn;
  const blocked = needsSignIn || (way === "tailscale" && !access?.tailscale_installed);

  return (
    <div className="pb-sheet" role="dialog" aria-label={t("access.title")}>
      <div className="pb-sheet-head">
        <strong>{t("access.title")}</strong>
        <button type="button" className="pb-close" onClick={onClose} aria-label={t("access.close")}>×</button>
      </div>
      <p className="pb-muted">{t("access.intro")}</p>

      <fieldset className="pb-group">
        <legend>{t("access.way")}</legend>
        {(["local", "tunnel", "tailscale"] as Way[]).map((option) => (
          <label key={option} className="pb-option">
            <input type="radio" name="way" checked={way === option} onChange={() => setWay(option)} />
            <span>
              <span className="pb-option-title">{t(`access.way.${option}`)}</span>
              <span className="pb-muted">{t(`access.way.${option}.hint`)}</span>
            </span>
          </label>
        ))}
      </fieldset>

      {way === "tunnel" ? (
        <div className="pb-group">
          {signedIn ? (
            <p className="pb-ok">{t("access.account.signedIn")}</p>
          ) : (
            <>
              <p className="pb-muted">{t("access.account.why")}</p>
              <div className="pb-row">
                <input className="pb-input" type="email" placeholder={t("access.account.email")} value={email}
                  onChange={(event) => setEmail(event.target.value)} />
                <Button size="sm" variant="outline" loading={phase === "sending"}
                  onClick={() => void call("account.login", { email })}>{t("access.account.send")}</Button>
              </div>
              {phase === "code_sent" || phase === "verifying" ? (
                <div className="pb-row">
                  <input className="pb-input" inputMode="numeric" placeholder={t("access.account.code")} value={code}
                    onChange={(event) => setCode(event.target.value)} />
                  <Button size="sm" loading={phase === "verifying"}
                    onClick={() => void call("account.code", { code })}>{t("access.account.verify")}</Button>
                </div>
              ) : null}
              {phase === "code_sent" ? <p className="pb-muted">{t("access.account.checkEmail")}</p> : null}
              {phase === "failed" && access?.login.error ? <p className="pb-error">{access.login.error}</p> : null}
            </>
          )}
        </div>
      ) : null}
      {way === "tailscale" && access && !access.tailscale_installed ? (
        <p className="pb-error">{t("access.tailscale.missing")}</p>
      ) : null}

      <fieldset className="pb-group">
        <legend>{t("access.lan")}</legend>
        <label className="pb-option">
          <input type="checkbox" checked={lan} onChange={(event) => setLan(event.target.checked)} />
          <span>
            <span className="pb-option-title">{t("access.lan.label")}</span>
            <span className="pb-muted">{t("access.lan.hint")}</span>
          </span>
        </label>
      </fieldset>

      {way !== "local" || lan ? (
        <label className="pb-row">
          <span className="pb-option-title">{t("access.port")}</span>
          <input className="pb-input pb-port" inputMode="numeric" value={port} onChange={(event) => setPort(event.target.value)} />
        </label>
      ) : null}

      {error ? <p className="pb-error">{error}</p> : null}
      {saved ? <p className="pb-ok">{t("access.saved")}</p> : null}
      <div className="pb-actions">
        <Button variant="ghost" onClick={onClose}>{t("access.close")}</Button>
        <Button loading={saving} disabled={blocked} onClick={save}>{t("access.save")}</Button>
      </div>
    </div>
  );
}
