const SESSION_ID = /^[A-Za-z0-9_-]+$/;
const MEDIA_EXTENSION = /^(png|jpe?g|webp|gif)$/i;
const MEDIA_PATH =
  /(?:^|[^\w/])((?:maps|hotels)\/[A-Za-z0-9_-]+\/(?:[A-Za-z0-9_-][A-Za-z0-9_.-]*\/)*[A-Za-z0-9_-][A-Za-z0-9_.-]*\.(?:png|jpe?g|webp|gif))(?=$|[^\w/?.#:\\])/gi;

export interface ParsedMediaPath {
  path: string;
  kind: "maps" | "hotels";
  sid: string;
  relativePath: string;
}

export interface MediaMatch {
  path: string;
  start: number;
  end: number;
  parsed: ParsedMediaPath;
}

/** Parse one complete, session-owned artifact path and return its canonical form. */
export function parseMediaPath(value: string, currentSid: string): ParsedMediaPath | null {
  if (!value || !SESSION_ID.test(currentSid) || /[\\\s]/.test(value)) return null;

  const match = value.match(
    /^(maps|hotels)\/([A-Za-z0-9_-]+)\/((?:[A-Za-z0-9_-][A-Za-z0-9_.-]*\/)*[A-Za-z0-9_-][A-Za-z0-9_.-]*)\.(png|jpe?g|webp|gif)$/i,
  );
  if (!match || match[1] !== match[1].toLowerCase() || match[2] !== currentSid) return null;

  const extension = match[4].toLowerCase();
  if (!MEDIA_EXTENSION.test(extension)) return null;
  const relativePath = `${match[3]}.${extension}`;
  return {
    path: `${match[1]}/${match[2]}/${relativePath}`,
    kind: match[1] as "maps" | "hotels",
    sid: match[2],
    relativePath,
  };
}

/** Find complete artifact tokens in prose, rejecting malformed path suffixes. */
export function findMediaMatches(text: string, currentSid: string): MediaMatch[] {
  if (!text || !SESSION_ID.test(currentSid)) return [];

  const matches: MediaMatch[] = [];
  for (const match of text.matchAll(MEDIA_PATH)) {
    const rawPath = match[1];
    const parsed = rawPath ? parseMediaPath(rawPath, currentSid) : null;
    if (!parsed || match.index === undefined) continue;
    const start = match.index + match[0].lastIndexOf(rawPath);
    matches.push({ path: parsed.path, start, end: start + rawPath.length, parsed });
  }
  return matches;
}

export function extractMediaPaths(text: string, currentSid: string): string[] {
  return [...new Set(findMediaMatches(text, currentSid).map((match) => match.path))];
}

/** Turn bare artifact paths in assistant prose into Markdown images once per path. */
export function replaceMediaPaths(
  source: string,
  currentSid: string | undefined,
  excludedPaths: ReadonlySet<string> = new Set<string>(),
): string {
  if (!currentSid) return source;

  const seen = new Set<string>();
  const matches = findMediaMatches(source, currentSid);
  let cursor = 0;
  let output = "";
  for (const match of matches) {
    output += source.slice(cursor, match.start);
    const prefix = source.slice(Math.max(0, match.start - 2), match.start);
    const excludedImage = excludedPaths.has(match.path)
      ? source.slice(0, match.start).match(/!\[([^\]]*)\]\($/)
      : null;
    if (excludedImage) {
      output = output.slice(0, -excludedImage[0].length);
      output += excludedImage[1];
      cursor = match.end + (source[match.end] === ")" ? 1 : 0);
      continue;
    }
    if (
      excludedPaths.has(match.path) ||
      seen.has(match.path) ||
      prefix === "](" ||
      prefix.endsWith("(")
    ) {
      output += source.slice(match.start, match.end);
    } else {
      seen.add(match.path);
      output += `\n\n![旅行产物](${match.path})\n\n`;
    }
    cursor = match.end;
  }
  return matches.length ? output + source.slice(cursor) : source;
}
