# Triaje de una solicitud de soporte

Analiza {{key}} para que una persona pueda decidir qué hacer. Esta sesión es
de investigación: no implementes soluciones, abras PRs, armes merges, cambies
el estado del ticket ni envíes mensajes al cliente. No ejecutes `/fix-ticket`
ni `/ship-pr`. Las instrucciones de entrega de código aplican a otra tarea.

Lee el ticket, sus relaciones y el código relevante. Distingue lo que dice el
cliente, lo que comprobaste en el código y lo que sigue siendo una hipótesis.
Si tienes acceso a comentarios y adjuntos mediante las herramientas configuradas,
consúltalos; si no, explica qué información faltó. No inventes reproducciones.

Puedes ejecutar pruebas existentes en el entorno aislado del worktree cuando
ayuden a confirmar una hipótesis. No modifiques producción ni ejecutes pruebas
destructivas. Anota el comando y su resultado. Una prueba no ejecutada debe
identificarse como propuesta. Verifica si la versión afectada coincide con el
código inspeccionado.

Escribe el informe en español en un archivo dentro de `POWERQUEUE_TASK_DIR`
(fuera de los archivos versionados del worktree), con estas secciones:

1. **Qué entiendo:** síntoma, usuario afectado, módulo e impacto.
2. **Evidencia:** rutas y símbolos relevantes, pruebas ejecutadas, relaciones
   con otros tickets, revisión inspeccionada y límites de la investigación.
3. **Hipótesis:** causa probable y qué evidencia la confirmaría o descartaría.
4. **Siguiente acción recomendada:** una decisión concreta y su justificación.
5. **Información faltante:** preguntas específicas y un borrador de respuesta
   al cliente. El borrador es una nota interna; no lo envíes.
6. **Preparación para implementación:** ¿hay suficiente información para una
   tarea de código? Si la hay, incluye alcance y criterios de aceptación; si
   no, explica qué impide definirlos. No crees esa tarea automáticamente.

Selecciona un resultado: `needs-information`, `needs-decision`,
`ready-for-agent`, `manual-action` o `no-action-recommended`.
`ready-for-agent` recomienda implementación, pero no la autoriza.

Completa usando el protocolo de Powerqueue con `--summary-file` y `--outcome`.
Powerqueue publica el informe y la etiqueta; no los publiques por tu cuenta.
La falta de datos del cliente es un resultado `needs-information`, no una razón
para dejar una sesión ocupando el slot mientras espera respuesta.
