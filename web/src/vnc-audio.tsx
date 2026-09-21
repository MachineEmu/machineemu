import { useCallback, useEffect, useRef, useState } from "react";
import { SpiceAudioClient, type AudioStatus } from "./spice-audio/client";
import { MachineEmuClient } from "./client";

type Props = { instanceId: string; sessionId: string };

type Availability = { available: boolean; reason: string | null; capture_held: boolean };

/**
 * Speaker and microphone controls for the VNC toolbar.
 *
 * Audio lives beside the display rather than in a tab of its own: it is part
 * of using the guest, not a separate view, and a tab that has to stay open
 * for the sound to keep playing is a trap.
 *
 * Nothing starts on mount. An `AudioContext` created outside a user gesture
 * is born suspended, and asking for the microphone on page load trains people
 * to dismiss the prompt.
 */
export function VncAudio({ instanceId, sessionId }: Props) {
  const client = useRef<SpiceAudioClient | null>(null);
  const [availability, setAvailability] = useState<Availability | null>(null);
  const [status, setStatus] = useState<AudioStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [contended, setContended] = useState(false);

  useEffect(() => {
    let active = true;
    void new MachineEmuClient({ token: "" }).audioStatus(instanceId, sessionId)
      .then((result) => {
        if (active) setAvailability({ available: result.available, reason: result.reason ?? null, capture_held: result.capture_held });
      })
      .catch(() => {
        if (active) setAvailability({ available: false, reason: null, capture_held: false });
      });
    return () => {
      active = false;
    };
  }, [instanceId, sessionId]);

  useEffect(
    () => () => {
      void client.current?.stop();
      client.current = null;
    },
    [sessionId]
  );

  const run = useCallback(async (work: () => Promise<void>) => {
    setBusy(true);
    try {
      await work();
    } finally {
      setBusy(false);
    }
  }, []);

  if (!availability?.available) {
    return availability === null ? null : (
      <span className="vnc-audio-note" title={availability.reason ?? undefined}>
        Guest audio unavailable
      </span>
    );
  }

  const connected = status?.state === "connected";

  async function connect() {
    const audio = new SpiceAudioClient(instanceId, sessionId, setStatus);
    client.current = audio;
    await audio.start();
  }

  async function disconnect() {
    await client.current?.stop();
    client.current = null;
    setStatus(null);
    setContended(false);
  }

  async function microphone(takeover: boolean) {
    if (!client.current) return;
    if (status?.capture) {
      await client.current.disableMicrophone();
      setContended(false);
      return;
    }
    try {
      await client.current.enableMicrophone(takeover);
      setContended(false);
    } catch (error) {
      // A 409 means another viewer holds the session's one capture lease.
      setContended(error instanceof Error && /another viewer/i.test(error.message));
      throw error;
    }
  }

  return (
    <div className="vnc-audio" aria-label="Guest audio">
      <button
        className={connected ? "quiet" : "primary"}
        disabled={busy || status?.state === "connecting"}
        onClick={() => void run(connected ? disconnect : connect)}
        title={
          connected
            ? "Stop forwarding guest audio"
            : "Forward the guest's speakers to this browser, and optionally your microphone to the guest"
        }
      >
        {status?.state === "connecting"
          ? "Connecting audio…"
          : connected
            ? "Stop audio"
            : "Start audio"}
      </button>
      {connected && (
        <>
          <button
            className="quiet"
            aria-pressed={status?.speakerMuted}
            onClick={() => client.current?.setSpeakerMuted(!status?.speakerMuted)}
            title="Mute the guest's audio in this browser"
          >
            {status?.speakerMuted ? "Speaker off" : "Speaker on"}
          </button>
          <button
            className={status?.capture ? "quiet" : "primary"}
            disabled={busy}
            onClick={() => void run(() => microphone(contended))}
            title={
              status?.capture
                ? "Stop sending your microphone to the guest"
                : contended
                  ? "Another viewer holds the microphone; click again to take it"
                  : "Send your microphone to the guest"
            }
          >
            {status?.capture ? "Stop microphone" : contended ? "Take microphone" : "Use microphone"}
          </button>
          {status?.capture && (
            <button
              className="quiet"
              aria-pressed={status?.microphoneMuted}
              onClick={() => client.current?.setMicrophoneMuted(!status?.microphoneMuted)}
            >
              {status?.microphoneMuted ? "Mic muted" : "Mic live"}
            </button>
          )}
          <span className="vnc-audio-note">
            {status?.capture
              ? status.guestListening
                ? "the guest is listening"
                : "waiting for the guest to open its microphone"
              : "speakers only"}
          </span>
        </>
      )}
      {status?.error && (
        <span className="vnc-audio-error" role="status">
          {status.error}
        </span>
      )}
    </div>
  );
}
