/**
 * Korean initial consonant (초성) + multi-word matcher matching masil's
 * `masil-agent agent find` behavior (`ㄱㅂ` matches `개발서버`).
 */
const CHOSEONG = [
  "ㄱ",
  "ㄲ",
  "ㄴ",
  "ㄷ",
  "ㄸ",
  "ㄹ",
  "ㅁ",
  "ㅂ",
  "ㅃ",
  "ㅅ",
  "ㅆ",
  "ㅇ",
  "ㅈ",
  "ㅉ",
  "ㅊ",
  "ㅋ",
  "ㅌ",
  "ㅍ",
  "ㅎ",
];

const CHOSEONG_SET = new Set(CHOSEONG);

export function extractChoseong(text: string): string {
  let out = "";
  for (const ch of text) {
    const code = ch.charCodeAt(0);
    if (code >= 0xac00 && code <= 0xd7a3) {
      const idx = Math.floor((code - 0xac00) / 588);
      out += CHOSEONG[idx] ?? ch;
    } else {
      out += ch.toLowerCase();
    }
  }
  return out;
}

function isChoseongWord(word: string): boolean {
  for (const ch of word) {
    if (!CHOSEONG_SET.has(ch)) return false;
  }
  return word.length > 0;
}

export function matchesMasilQuery(haystack: string, query: string): boolean {
  const words = query
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  if (words.length === 0) return true;
  const lower = haystack.toLowerCase();
  const initials = extractChoseong(haystack);

  for (const w of words) {
    if (lower.includes(w)) continue;
    if (isChoseongWord(w) && initials.includes(w)) continue;
    return false;
  }
  return true;
}
