import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { copyToClipboard, dismissAskAnswer, getAskAnswer, type AskAnswer } from "../api";
import { translateAppMessage, useI18n } from "../i18n";

export function AskAnswerPanel() {
  const { language, t } = useI18n();
  const [answer, setAnswer] = useState("");
  const [answerKind, setAnswerKind] = useState<"generated" | "notice">("generated");
  const [operationId, setOperationId] = useState(0);
  const operationRef = useRef(0);
  useEffect(() => {
    let active = true;
    const setup = async () => {
      const unlisten = await listen<AskAnswer>("ask-answer", ({ payload }) => {
        if (!active || payload.operationId < operationRef.current) return;
        operationRef.current = payload.operationId;
        setOperationId(payload.operationId);
        setAnswer(payload.payload);
        setAnswerKind(payload.kind);
      });
      if (!active) {
        unlisten();
        return;
      }
      const current = await getAskAnswer();
      if (active && current && current.operationId >= operationRef.current) {
        operationRef.current = current.operationId;
        setOperationId(current.operationId);
        setAnswer(current.payload);
        setAnswerKind(current.kind);
      }
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
  return <main className="ask-answer-panel" aria-live="polite">
    <p className="eyebrow">{t("Ask Anything")}</p><div className="ask-answer-text">{displayedAnswer}</div>
    <div className="ask-answer-actions"><button className="secondary" type="button" disabled={!answer} onClick={() => void copyToClipboard(displayedAnswer)}>{t("Copy")}</button><button className="secondary" type="button" disabled={!answer} onClick={() => void dismiss()}>{t("Dismiss")}</button></div>
  </main>;
}

