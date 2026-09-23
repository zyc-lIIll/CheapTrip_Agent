import { apiRequest } from "../lib/http";
import type { AppMeta } from "./types";

export const metaApi = {
  get: () => apiRequest<AppMeta>("/api/meta"),
};
