import { useCallback, useEffect, useRef, useState } from "react";
import { listAudioDevices } from "../api";
import type { AudioDevice } from "../types";

export function useAudioDevices(active: boolean, onError: (error: unknown) => void) {
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const callbacks = useRef(onError);
  callbacks.current = onError;
  const request = useRef(0);
  const refresh = useCallback(async () => {
    const id = ++request.current;
    try { const next = await listAudioDevices(); if (id === request.current) setDevices(next); }
    catch (error) { if (id === request.current) callbacks.current(error); }
  }, []);
  useEffect(() => { void refresh(); return () => { ++request.current; }; }, [refresh]);
  useEffect(() => {
    if (!active) return;
    void refresh();
    const focus = () => { void refresh(); };
    window.addEventListener("focus", focus);
    return () => { window.removeEventListener("focus", focus); };
  }, [active, refresh]);
  return { devices, refresh };
}
