import { mount } from "svelte";
import Overlay from "./overlay/Overlay.svelte";

mount(Overlay, { target: document.getElementById("overlay")! });
