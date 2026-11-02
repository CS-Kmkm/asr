import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { copyToClipboard, dismissAskAnswer, getAskAnswer, type AskAnswer } from "../api";
import { translateAppMessage, useI18n } from "../i18n";
import type { AskAnswerPreview } from "../types";

type Preview = { operationId: number; text: string };

export function AskAnswerPanel() {
  const { language, t } = useI18n();
  const [answer, setAnswer] = useState("");
  const [answerKind, setAnswerKind] = useState<"generated" | "notice">("generated");
  const [operationId, setOperationId] = useState(0);
  const [preview, setPreview] = useState<Preview | null>(null);
  const operationRef = useRef(0);
  // Newest operation whose preview ended (final answer, failure or cancel):
  // later partials of it or of older operations are stale.
  const previewEndedRef = useRef(0);
  const previewRef = useRef(0);
  useEffect(() => {
    let active = true;
    const showAnswer = (value: AskAnswer) => {
      operationRef.current = value.operationId;
      previewEndedRef.current = Math.max(previewEndedRef.current, value.operationId);
      if (previewRef.current <= value.operationId) setPreview(null);
      setOperationId(value.operationId);
      setAnswer(value.payload);
      setAnswerKind(value.kind);
    };
    const setup = async () => {
      const unlistenAnswer = await listen<AskAnswer>("ask-answer", ({ payload }) => {
        if (!active || payload.operationId < operationRef.current) return;
        showAnswer(payload);
      });
      const unlistenPreview = await listen<AskAnswerPreview>("ask-answer-preview", ({ payload }) => {
        if (!active) return;
        if (!payload.active) {
          previewEndedRef.current = Math.max(previewEndedRef.current, payload.operationId);
          if (previewRef.current <= payload.operationId) setPreview(null);
          return;
        }
        if (payload.operationId <= previewEndedRef.current || payload.operationId < previewRef.current) return;
        previewRef.current = payload.operationId;
        setPreview({ operationId: payload.operationId, text: payload.text });
      });
      const unlisten = () => { unlistenAnswer(); unlistenPreview(); };
      if (!active) {
        unlisten();
        return;
      }
      const current = await getAskAnswer();
      if (active && current && current.operationId >= operationRef.current) showAnswer(current);
      return unlisten;
    };
    let unlisten: (() => void) | undefined;
    void setup().then((cleanup) => { unlisten = cleanup; if (!active) cleanup?.(); });
    return () => { active = false; unlisten?.(); };
  }, []);
  const dismiss = async () => {
    await dismissAskAnswer(operationId);
  };
  const displayedAnswer = answerKind === "notice" ? translateAppMessage(language, answer) ?? answer : answer;
  const previewing = preview !== null;
  // Partial text is never copyable or announced: only the validated answer
  // enters the polite live region, once, when it replaces the preview.
  return <main className="ask-answer-panel" aria-busy={previewing}>
    <p className="eyebrow">{t("Ask Anything")}</p>
    {previewing && <>
      <p className="ask-answer-status">{t("Generating answer…")}</p>
      <div className="ask-answer-text ask-answer-preview" aria-live="off" data-operation-id={preview.operationId}>{preview.text}</div>
    </>}
    <div className="ask-answer-text" aria-live="polite" hidden={previewing}>{displayedAnswer}</div>
    <div className="ask-answer-actions"><button className="secondary" type="button" disabled={!answer || previewing} onClick={() => void copyToClipboard(displayedAnswer)}>{t("Copy")}</button><button className="secondary" type="button" disabled={!answer || previewing} onClick={() => void dismiss()}>{t("Dismiss")}</button></div>
  </main>;
}
