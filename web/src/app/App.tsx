import { lazy, Suspense } from "react";
import { Navigate, Route, Routes } from "react-router-dom";

import {
  AdminRoute,
  ChangePasswordRoute,
  LoginRoute,
  ProtectedRoute,
} from "./AuthProvider";

const ChatPage = lazy(() => import("../pages/ChatPage").then((module) => ({ default: module.ChatPage })));
const LoginPage = lazy(() => import("../pages/LoginPage").then((module) => ({ default: module.LoginPage })));
const ChangePasswordPage = lazy(() =>
  import("../pages/ChangePasswordPage").then((module) => ({ default: module.ChangePasswordPage })),
);
const AdminSettingsPage = lazy(() =>
  import("../pages/admin/AdminSettingsPage").then((module) => ({ default: module.AdminSettingsPage })),
);
const AdminUsersPage = lazy(() =>
  import("../pages/admin/AdminUsersPage").then((module) => ({ default: module.AdminUsersPage })),
);

export function App() {
  return (
    <Suspense fallback={<main aria-busy="true">正在加载…</main>}>
      <Routes>
        <Route path="/" element={<ProtectedRoute><ChatPage /></ProtectedRoute>} />
        <Route path="/login" element={<LoginRoute><LoginPage /></LoginRoute>} />
        <Route path="/change-password" element={<ChangePasswordRoute><ChangePasswordPage /></ChangePasswordRoute>} />
        <Route
          path="/admin/settings"
          element={<ProtectedRoute><AdminRoute><AdminSettingsPage /></AdminRoute></ProtectedRoute>}
        />
        <Route
          path="/admin/users"
          element={<ProtectedRoute><AdminRoute><AdminUsersPage /></AdminRoute></ProtectedRoute>}
        />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </Suspense>
  );
}
