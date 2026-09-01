local pager = {}

local function page_end(view)
    local cursor = view.cursor
    if cursor.visible and cursor.row == view.rows - 1 then
        return {row = view.rows - 1, col = 0}
    end
    return view:bottom()
end

function pager.read()
    local view = lector.api.view()
    local reader = lector.api.reader()
    local first = view.review
    local stop_after_page = false

    while true do
        local result = reader:read(view, first, page_end(view))
        if result.status ~= "completed" then
            return
        end

        if stop_after_page then
            break
        end

        local response = view:send_keys("<C-f>"):wait_for_stable_screen()
        if response.status ~= "presented" or not response.content_changed then
            break
        end

        view = response.view
        first = view:top()
        stop_after_page = response.effects.bells > 0
    end

    reader:close()
end

return pager
