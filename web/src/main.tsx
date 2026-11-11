import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { router } from "./router";
import { ApiError } from "./api/client";
import { applyTheme, preferredTheme } from "./theme";
import "./styles.css";

// index.html sets the theme before first paint; this keeps the meta color in step
// when the page is opened without that script (for example in tests).
applyTheme(preferredTheme());

const client = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 2000,
      retry: (count, error) =>
        count < 1 && (!(error instanceof ApiError) || error.status >= 500),
      refetchOnWindowFocus: true,
    },
    mutations: { retry: false },
  },
});
ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={client}>
      <a className="skip-link" href="#main-content">
        본문으로 건너뛰기
      </a>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </React.StrictMode>,
);
