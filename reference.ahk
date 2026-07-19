#Requires AutoHotkey v2.0
#SingleInstance force

CapsEventOccurred := False

CapsSend(key) {
    global CapsEventOccurred
    CapsEventOccurred := True
    Send(key)
}

CapsLock:: {
    global CapsEventOccurred
    CapsEventOccurred := False
    if (KeyWait("CapsLock", "T0.3") and !CapsEventOccurred) {
        Send("{Esc}")
    }
    KeyWait("CapsLock")
    Return
}

#HotIf GetKeyState("CapsLock", "P")
e::CapsSend("{up}")
s::CapsSend("{left}")
d::CapsSend("{down}")
f::CapsSend("{right}")

a::CapsSend("^{left}")
g::CapsSend("^{right}")

w::CapsSend("{Backspace}")
r::CapsSend("{Delete}")

t::CapsSend("{up 5}")
b::CapsSend("{down 5}")

i::CapsSend("+{up}")
j::CapsSend("+{left}")
k::CapsSend("+{down}")
l::CapsSend("+{right}")

h::CapsSend("^+{left}")
.::CapsSend("^+{right}")

u::CapsSend("+{Home}")
o::CapsSend("+{End}")

p::CapsSend("{Home}")
`;::CapsSend("{End}")

y::CapsSend("+{up 5}")
n::CapsSend("+{down 5}")

; Custom hotkeys
q::CapsSend("!+q")
/::CapsSend("!+/")
Space::CapsSend("!+]")
#HotIf

; Other hotkeys: mouse auto-clicker
F13:: {
    While GetKeyState("F13", "P") {
        MouseClick("left") 
        sleep(10)
    }
}
F14:: {
    While GetKeyState("F14", "P") {
        MouseClick("right") 
        sleep(10)
    }
}
F15:: {
    While GetKeyState("F15", "P") {
        MouseClick("Middle") 
        sleep(10)
    }
}
