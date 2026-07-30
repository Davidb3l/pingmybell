import { mount } from "svelte";
import App from "./board/App.svelte";

mount(App, { target: document.getElementById("app")! });
