import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { en } from "./en";
import { ko } from "./ko";

let initialized = false;

export function initI18n(defaultLang: "en" | "ko" = "ko"): typeof i18n {
  if (!initialized) {
    initialized = true;
    void i18n.use(initReactI18next).init({
      lng: defaultLang,
      fallbackLng: "en",
      resources: {
        en: { translation: en },
        ko: { translation: ko },
      },
      interpolation: {
        escapeValue: false,
      },
    });
  }
  return i18n;
}

export { i18n };
