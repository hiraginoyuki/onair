import App from "./App.svelte";
import "./app.css";
import { mount } from "svelte";

const target = document.getElementById("app");
if (!target) {
  throw new Error("inspector app mount point is missing");
}

mount(App, { target });
