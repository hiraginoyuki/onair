import App from "./App.svelte";
import "./app.css";

const target = document.getElementById("app");
if (!target) {
  throw new Error("inspector app mount point is missing");
}

new App({ target });
